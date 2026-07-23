use awaken_runtime_host::WorkerUpstream;
use awaken_worker::{WorkerNodeBuilder, WorkerShutdown};
use awaken_worker_contract::{VersionRange, WorkerManifest};

mod support;
use support::FakeWorkerUpstream;

fn manifest() -> WorkerManifest {
    WorkerManifest {
        build_digest: "worker-node-test".to_string(),
        dispatch_contract: VersionRange::exact(1),
        runtime_protocol: VersionRange::exact(1),
        ..WorkerManifest::default()
    }
}

#[test]
fn builder_rejects_incomplete_or_invalid_topology() {
    let missing = WorkerNodeBuilder::new(WorkerUpstream::new("http://control"))
        .build()
        .err()
        .expect("manifest is mandatory");
    assert!(missing.to_string().contains("manifest"));

    let empty_upstream = WorkerNodeBuilder::new(WorkerUpstream::new(""))
        .with_manifest(manifest())
        .build()
        .err()
        .expect("empty upstream is invalid");
    assert!(empty_upstream.to_string().contains("upstream"));

    let mut zero_capacity = manifest();
    zero_capacity.capacity.max_concurrent = 0;
    let invalid_capacity = WorkerNodeBuilder::new(WorkerUpstream::new("http://control"))
        .with_manifest(zero_capacity)
        .build()
        .err()
        .expect("zero capacity is invalid");
    assert!(invalid_capacity.to_string().contains("max_concurrent"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_runs_register_ready_drain_quiesce_and_deregister() {
    let upstream = FakeWorkerUpstream::start();
    WorkerNodeBuilder::new(WorkerUpstream::new(upstream.url()).with_worker_id("worker-node-test"))
        .with_manifest(manifest())
        .without_admin_surface()
        .build()
        .expect("valid explicit Worker topology")
        .run_until(async { Ok(WorkerShutdown::Prompt) })
        .await
        .expect("Worker lifecycle completes");

    let requests = upstream.requests();
    let positions: Vec<_> = [
        "/v1/worker/register",
        "/v1/worker/heartbeat",
        "/v1/worker/drain",
        "/v1/worker/quiesced",
        "/v1/worker/deregister",
    ]
    .into_iter()
    .map(|path| {
        requests
            .iter()
            .position(|request| request == path)
            .unwrap_or_else(|| panic!("missing {path} in {requests:?}"))
    })
    .collect();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "Worker lifecycle calls are ordered: {requests:?}"
    );
}
