//! Retirement authenticates the existing owner without reviving Work.
use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn signed_retirement_keeps_expired_proof_and_never_acquires_retired_work() {
    // R1 current signed incarnation + exact owner + expired phase + retired Work
    // reaches Root Control once, even while Draining. R2 wrong incarnation or
    // foreign lease rejects before Control. R3 unknown command fields reject at
    // the original serde boundary. Every row has zero Work acquisitions: retire
    // revokes an issued phase and cannot revive execution merely to close it.
    let clock = Arc::new(ManualClock::new(10_000));
    let credential = WorkerSigningCredential::new(
        "retiring-worker",
        "rotation",
        "credential",
        b"retirement-fixture-secret".to_vec(),
    )
    .unwrap();
    let authenticator: Arc<dyn WorkerRequestAuthenticator> = Arc::new(
        SignedWorkerAuthenticator::new(credential.clone(), clock.clone())
            .with_time_policy(60_000, 0),
    );
    let directory = Arc::new(awaken_worker_registry::MemoryWorkerDirectory::new());
    let control = Arc::new(RecordingSessionControl::default());
    let work = Arc::new(RecordingSessionWorkAuthority::default());
    work.retired.store(true, Ordering::SeqCst);
    let service = WorkerDispatchService::new(RegisteredDispatchDependencies {
        authenticator,
        realizations: control.clone(),
        session_work: work.clone(),
        ..worker_dispatch_dependencies(
            Arc::new(MemoryDispatchStore::new(clock.clone())),
            directory,
            clock.clone(),
        )
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            dispatch_transport_router_with_service(Arc::new(service)),
        )
        .await
        .unwrap()
    });
    let bootstrap = WorkerUpstream::new(format!("http://{address}"))
        .with_worker_id("retiring-worker")
        .with_request_authorizer(Arc::new(SignedWorkerRequestAuthorizer::new(
            credential,
            clock.clone(),
        )));
    let lifecycle = WorkerControlClient::new(bootstrap.clone());
    let identity = lifecycle
        .register_classified("retirement-boot", current_dispatch_codec_manifest())
        .await
        .unwrap()
        .worker
        .snapshot
        .identity;
    // Registration is bootstrap-signed; every later request must sign the
    // exact returned incarnation as well as carry it in its body.
    let upstream = bootstrap.with_worker_identity(identity.clone());
    let lifecycle = WorkerControlClient::new(upstream.clone());
    lifecycle
        .heartbeat(
            &identity,
            WorkerHeartbeat {
                sequence: 1,
                ready: true,
                in_flight: 0,
                warm_environment_shapes: Default::default(),
                credential_observations: Default::default(),
                acp_capability_observations: Default::default(),
                source_retention: Default::default(),
            },
        )
        .await
        .unwrap();
    let path = "/v1/worker/session/realization/retire";
    let command = awaken_session_execution_contract::RetireSessionRealization {
        session_id: "retired-session".into(),
        lease: awaken_session_configuration_contract::resource::SessionRealizationLease {
            owner: identity.worker_id.clone(),
            runtime_incarnation: identity.lease_owner(),
            epoch: 7,
            expires_at_unix_ms: 0,
        },
    };
    for draining in [false, true] {
        if draining {
            lifecycle.begin_drain(&identity, None).await.unwrap();
        }
        let response = upstream
            .authorize(
                "POST",
                path,
                upstream.client().post(format!("http://{address}{path}")),
            )
            .unwrap()
            .json(&serde_json::json!({ "identity": identity, "command": command }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            reqwest::StatusCode::OK,
            "R1 draining={draining}"
        );
        assert_eq!(
            response.json::<serde_json::Value>().await.unwrap(),
            serde_json::json!({"disposition":"retired"})
        );
    }
    assert_eq!(
        control.retirements.lock().unwrap().as_slice(),
        &[command.clone(), command.clone()]
    );
    let mut wrong_incarnation = serde_json::json!({ "identity": identity, "command": command });
    wrong_incarnation["identity"]["incarnation_id"] = serde_json::json!("foreign-boot");
    let mut foreign_lease = serde_json::json!({ "identity": identity, "command": command });
    foreign_lease["command"]["lease"]["owner"] = serde_json::json!("foreign-worker");
    let mut unknown = serde_json::json!({ "identity": identity, "command": command });
    unknown["command"]["unknown"] = serde_json::json!(true);
    for (body, status) in [
        (wrong_incarnation, reqwest::StatusCode::BAD_REQUEST),
        (foreign_lease, reqwest::StatusCode::BAD_REQUEST),
        (unknown, reqwest::StatusCode::UNPROCESSABLE_ENTITY),
    ] {
        let response = upstream
            .authorize(
                "POST",
                path,
                upstream.client().post(format!("http://{address}{path}")),
            )
            .unwrap()
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status, "R2/R3");
        assert_eq!(
            control.retirements.lock().unwrap().len(),
            2,
            "zero extra Root calls"
        );
    }
    assert_eq!(work.acquisitions.load(Ordering::SeqCst), 0);
    assert_eq!(work.releases.load(Ordering::SeqCst), 0);
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}
