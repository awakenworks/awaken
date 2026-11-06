//! T23-C02 oracle 2: the Dream source snapshot is read only behind the source
//! store's authorization boundary.
use super::*;
use awaken_resource_contract::MemErr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Counts every source snapshot read while delegating to the real selected source.
struct CountingSnapshots {
    original: Arc<dyn MemorySnapshotSource>,
    reads: AtomicUsize,
    pause_after_read: AtomicBool,
    captured: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl MemorySnapshotSource for CountingSnapshots {
    async fn snapshot_heads(&self, store: &str) -> Result<Vec<Memory>, MemErr> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let files = self.original.snapshot_heads(store).await?;
        if self.pause_after_read.swap(false, Ordering::SeqCst) {
            self.captured.notify_one();
            self.resume.notified().await;
        }
        Ok(files)
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
        pause_after_read: AtomicBool::new(false),
        captured: tokio::sync::Notify::new(),
        resume: tokio::sync::Notify::new(),
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

#[tokio::test]
async fn prepare_re_resolves_the_source_before_its_snapshot_read() {
    // C1 validation saw an Active source; C2 deletion commits before prepare.
    // R1 C1+C2 => input_memory_store_unavailable, zero source reads, no private
    // catalog entries and no newly admitted Session. Earlier validation is not
    // an authorization receipt that survives a later lifecycle change.
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
    for store in ["mem_snapshot_auth-toctou", "mem_result_auth-toctou"] {
        assert!(
            fixture
                .resources
                .memory_stores()
                .get("default", store)
                .await
                .unwrap()
                .is_none(),
            "R1 no private catalog entry"
        );
    }
    assert!(
        fixture
            .agent
            .sessions
            .queries()
            .session_transcript("default", "sesn_dream_auth-toctou")
            .await
            .is_err(),
        "R1 no Session admitted"
    );
}

#[tokio::test]
async fn source_snapshot_requires_a_current_active_source_in_the_request_workspace() {
    // | Rule | definition | Workspace | lifecycle | Effect                 |
    // | S1   | exists     | exact     | Active    | one complete snapshot  |
    // | S2   | exists     | exact     | Suspended | refusal, zero reads    |
    // | S3   | exists     | foreign   | Active    | refusal, zero reads    |
    // | S4   | absent     | exact     | -         | refusal, zero reads    |
    // Source reads use the same resolver as validate_inputs and prepare.
    for (rule, source, workspace, state, allowed) in [
        ("S1", "dream-source", "default", ResourceState::Active, true),
        (
            "S2",
            "dream-source",
            "default",
            ResourceState::Suspended,
            false,
        ),
        (
            "S3",
            "dream-source",
            "foreign",
            ResourceState::Active,
            false,
        ),
        ("S4", "missing", "default", ResourceState::Active, false),
    ] {
        let fixture = fixture().await;
        if state != ResourceState::Active {
            fixture
                .resources
                .memory_stores()
                .set_state("default", "dream-source", state)
                .await
                .unwrap();
        }
        let mut request = request(rule, source);
        request.workspace_id = workspace.into();
        let snapshot = fixture.agent.source_snapshot(&request).await;
        if allowed {
            let files = snapshot.expect("S1 source remains authorized across the read");
            assert_eq!(files.len(), 1, "{rule}");
            assert_eq!(
                files[0].content.as_deref(),
                Some("source sentinel"),
                "{rule}"
            );
        } else {
            assert_eq!(
                snapshot.unwrap_err().kind,
                "input_memory_store_unavailable",
                "{rule}"
            );
        }
        assert_eq!(
            fixture.counter.reads.load(Ordering::SeqCst),
            usize::from(allowed),
            "{rule}"
        );
    }
}

#[tokio::test]
async fn a_source_revoked_while_its_snapshot_is_read_is_not_cloned() {
    // C1 the real content snapshot is captured; C2 its return is held while
    // deletion commits; C3 the captured bytes return to prepare. R1 C1+C2+C3
    // => typed refusal before any clone/output/Session, and a private snapshot
    // remains Suspended. Notify barriers order the race without a timed sleep.
    let fixture = fixture().await;
    let request = request("auth-during-read", "dream-source");
    fixture
        .counter
        .pause_after_read
        .store(true, Ordering::SeqCst);
    let (prepared, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(fixture.agent.prepare(&request), async {
            fixture.counter.captured.notified().await;
            fixture
                .resources
                .memory_stores()
                .delete("default", "dream-source", 1)
                .await
                .unwrap();
            fixture.counter.resume.notify_one();
        })
    })
    .await
    .expect("R1 both sides of the controlled revocation settle");
    assert_eq!(
        prepared.unwrap_err().kind,
        "input_memory_store_unavailable",
        "R1"
    );
    assert_eq!(
        fixture.counter.reads.load(Ordering::SeqCst),
        1,
        "R1 one held source read"
    );
    let stores = fixture.resources.memory_stores();
    assert_eq!(
        stores
            .get("default", "mem_snapshot_auth-during-read")
            .await
            .unwrap()
            .unwrap()
            .state,
        ResourceState::Suspended,
        "R1 no visible partial input"
    );
    assert!(
        stores
            .get("default", "mem_result_auth-during-read")
            .await
            .unwrap()
            .is_none(),
        "R1 no output"
    );
    assert!(
        fixture
            .resources
            .authorities()
            .memory_snapshot_source()
            .snapshot_heads("mem_snapshot_auth-during-read")
            .await
            .unwrap()
            .is_empty(),
        "R1 no unauthorized bytes cloned"
    );
    assert!(
        fixture
            .agent
            .sessions
            .queries()
            .session_transcript("default", "sesn_dream_auth-during-read")
            .await
            .is_err(),
        "R1 no Session admitted"
    );
}
