use std::sync::Arc;

use awaken_coordinator_client::WorkerControlClient;
use awaken_scenario_host::{EchoModel, build_router_with_deployment};
use awaken_worker_registry::{WorkerIdentity, WorkerManifest};
use awaken_worker_transport_security::WorkerUpstream;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

async fn serve(app: axum::Router) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind Scenario Worker transport");
    let address = listener.local_addr().expect("Scenario listener address");
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve Scenario Worker transport");
    });
    (address, server)
}

#[tokio::test]
async fn scenario_worker_warmup_matches_private_transport_topology_and_identity_fence() {
    // Constraints/invariants: the Coordinator routes one neutral Session/Run lifecycle; live
    // delivery is best-effort and cannot replace committed replay truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Static cause/effect graph: C1 the Host has no local dispatch pool, C2 the
    // common Coordinator mount owns dispatch/resource/commit/warmup, C3 the
    // Environment source is passed directly from Scenario composition. Effects:
    // E1 Scenario exposes the one production Worker transport, E2 no second
    // warmup handler/catalog/queue is assembled. Production's component consumes
    // that returned transport once; a second merge of the same Axum route would
    // fail component construction and is covered by the split-role topology test.
    // This case checks default empty demand and HTTP identity isolation. Exact
    // Arc selection and source/refresh failure propagation are separately tested
    // in executable_projection_refresh_tests.
    //
    // Dynamic causes: C4 exact registered Worker identity, C5 unknown identity,
    // C6 a foreign authenticated Worker presents another Worker's identity, C7
    // local pool enabled. Effects: E3 C4 reads the authoritative warmup list;
    // E4 C5/C6 fail before Environment disclosure; E5 C7 keeps the private route
    // hidden on the single-listener Scenario surface.
    //
    // | Rule | remote-only | identity                 | Effect |
    // | W1   | yes         | exact current registered | E1+E2+E3 (200) |
    // | W2   | yes         | unknown                  | E4 (reject) |
    // | W3   | yes         | foreign current          | E4 (reject) |
    // | W4   | no          | any                      | E5 (404) |
    let mut remote = awaken_deployment_config::DeploymentConfig::ephemeral();
    remote.disable_local_pool = true;
    let remote =
        build_router_with_deployment(Arc::new(EchoModel), "warmup-parity", remote, false).await;
    let (address, server) = serve(remote).await;

    let exact = WorkerControlClient::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_id("warmup-worker"),
    );
    let registration = exact
        .register_classified(
            "warmup-boot",
            WorkerManifest {
                // W1 requires positive protocol evidence before identity admission.
                runtime_protocol: awaken_worker_contract::VersionRange::exact(
                    awaken_worker_contract::CURRENT_RUNTIME_PROTOCOL_VERSION,
                ),
                ..Default::default()
            },
        )
        .await
        .expect("W1 register exact Worker");
    let warmups = exact
        .current_environment_warmups(&registration.worker.snapshot.identity)
        .await
        .expect("W1 decode the canonical warmup response");
    assert!(
        warmups.is_empty(),
        "W1 default Scenario catalog has no warm demand"
    );

    let unknown = WorkerControlClient::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_id("unknown-worker"),
    );
    assert!(
        unknown
            .current_environment_warmups(&WorkerIdentity::new("unknown-worker", "unknown-boot", 1,))
            .await
            .is_err(),
        "W2 unknown identity is rejected"
    );

    let foreign = WorkerControlClient::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_id("foreign-worker"),
    );
    assert!(
        foreign
            .current_environment_warmups(&registration.worker.snapshot.identity)
            .await
            .is_err(),
        "W3 foreign Worker cannot reuse another identity"
    );
    server.abort();

    let local = build_router_with_deployment(
        Arc::new(EchoModel),
        "warmup-parity",
        awaken_deployment_config::DeploymentConfig::ephemeral(),
        false,
    )
    .await;
    let response = local
        .oneshot(
            Request::post("/v1/worker/environment/warmups")
                .header("content-type", "application/json")
                .header("x-awaken-worker-id", "local-worker")
                .body(Body::from("{}"))
                .expect("W4 request"),
        )
        .await
        .expect("W4 response");
    assert_eq!(response.status(), StatusCode::NOT_FOUND, "W4");
}
