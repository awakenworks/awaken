//! A claimed Run-free preparation takes its Session's Work before any effect.
use super::*;

use awaken_session_application_contract::{SessionWorkAcquisition, SessionWorkOwnership};

/// The selected Work observation one claim sees.
#[derive(Clone, Copy, PartialEq)]
enum WorkMode {
    NotRequired,
    Free,
    Foreign,
    Unowned,
    Unavailable,
}

struct PreparationWork {
    mode: Mutex<WorkMode>,
    causes: Mutex<Vec<SessionWorkAcquisition>>,
}

#[async_trait::async_trait]
impl awaken_session_application_contract::SessionWorkerLeaseAuthority for PreparationWork {
    async fn acquire_session_work(
        &self,
        session_id: &SessionId,
        worker_owner: &str,
        acquisition: SessionWorkAcquisition,
    ) -> Result<SessionWorkOwnership, awaken_runtime_contract::runs::RunError> {
        self.causes.lock().unwrap().push(acquisition);
        let leased = |owner: &str| {
            awaken_session_types::SessionWorkLease::from_persisted(
                "work-session",
                "env",
                session_id.clone(),
                owner,
                1,
                OBSERVED_WORK_EXPIRY_MS,
            )
            .map(awaken_session_application_contract::SessionWorkGrant::from_transfer)
            .map(SessionWorkOwnership::Leased)
            .expect("nonzero epoch")
        };
        match *self.mode.lock().unwrap() {
            WorkMode::NotRequired => Ok(SessionWorkOwnership::NotRequired),
            WorkMode::Free => Ok(leased(worker_owner)),
            WorkMode::Foreign => Ok(leased("another-worker:boot")),
            WorkMode::Unowned => Ok(SessionWorkOwnership::Unowned),
            WorkMode::Unavailable => Err(awaken_runtime_contract::runs::RunError::unavailable(
                "Work store is unavailable",
            )),
        }
    }

    async fn release_session_work(
        &self,
        _lease: &awaken_session_application_contract::SessionWorkGrant,
    ) -> Result<bool, awaken_runtime_contract::runs::RunError> {
        panic!("claiming a preparation never releases Work")
    }

    async fn release_worker_session_work(
        &self,
        _worker_owner: &str,
    ) -> Result<usize, awaken_runtime_contract::runs::RunError> {
        panic!("claiming a preparation never releases Work")
    }
}

/// The one Session-root selector: it always issues a grant for `prepared`.
struct IssuingPreparation {
    claims: AtomicUsize,
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionPreparationControl for IssuingPreparation {
    async fn claim_next_session_preparation(
        &self,
        target: awaken_session_execution_contract::SessionRealizationTarget,
    ) -> Result<
        Option<awaken_session_execution_contract::SessionPreparationAssignment>,
        awaken_session_execution_contract::SessionRealizationControlFailure,
    > {
        self.claims.fetch_add(1, Ordering::SeqCst);
        let lease = awaken_session_configuration_contract::resource::SessionRealizationLease {
            owner: target.owner,
            runtime_incarnation: target.runtime_incarnation,
            epoch: 3,
            expires_at_unix_ms: target.lease_expires_at_unix_ms,
        };
        Ok(Some(
            awaken_session_execution_contract::SessionPreparationAssignment {
                session_id: "prepared".into(),
                directive: awaken_session_execution_contract::SessionRealizationDirective {
                    projection: frozen_projection(),
                    authority:
                        awaken_session_execution_contract::SessionRealizationLeaseReceipt::issued_at(
                            lease, 10_000,
                        )?,
                    action: awaken_session_execution_contract::SessionRealizationAction::Complete,
                },
            },
        ))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_claimed_preparation_takes_its_session_work_or_retires_its_grant() {
    // Causes: C1 the Root selector issued a Run-free preparation grant to this
    // Ready Worker; the Session's Work observation is C2 not required, C3 free
    // (taken by this Worker), C4 held by another Worker, C5 unowned (stopped,
    // nothing to take) or C6 unreadable. Effects: E1 the claim takes Work with
    // the ClaimedPreparation cause; E2 the assignment reaches the Worker; E3 the
    // just-issued grant is retired through the Root and nothing is handed out;
    // E4 the failure is reported. Without E1 every later Environment effect of
    // the preparation is refused (renewal never takes Work), so the Session's
    // queued Run waits forever. Rules: A1=C1+C2=>E1+E2; A2=C1+C3=>E1+E2;
    // A3=C1+(C4|C5)=>E1+E3; A4=C1+C6=>E1+E3+E4.
    let clock = Arc::new(ManualClock::new(10_000));
    let credential = WorkerSigningCredential::new(
        "preparing-worker",
        "rotation",
        "credential",
        b"preparation-fixture-secret".to_vec(),
    )
    .unwrap();
    let authenticator: Arc<dyn WorkerRequestAuthenticator> = Arc::new(
        SignedWorkerAuthenticator::new(credential.clone(), clock.clone())
            .with_time_policy(60_000, 0),
    );
    let control = Arc::new(RecordingSessionControl::default());
    let preparation = Arc::new(IssuingPreparation {
        claims: AtomicUsize::new(0),
    });
    let work = Arc::new(PreparationWork {
        mode: Mutex::new(WorkMode::NotRequired),
        causes: Mutex::new(Vec::new()),
    });
    let service = WorkerDispatchService::new(RegisteredDispatchDependencies {
        authenticator,
        realizations: control.clone(),
        session_preparation: preparation.clone(),
        session_work: work.clone(),
        ..worker_dispatch_dependencies(
            Arc::new(MemoryDispatchStore::new(clock.clone())),
            Arc::new(awaken_worker_registry::MemoryWorkerDirectory::new()),
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
        .with_worker_id("preparing-worker")
        .with_request_authorizer(Arc::new(SignedWorkerRequestAuthorizer::new(
            credential,
            clock.clone(),
        )));
    let identity = WorkerControlClient::new(bootstrap.clone())
        .register_classified("preparation-boot", current_dispatch_codec_manifest())
        .await
        .unwrap()
        .worker
        .snapshot
        .identity;
    let lifecycle = WorkerControlClient::new(bootstrap.with_worker_identity(identity.clone()));
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
    let target = awaken_session_execution_contract::SessionRealizationTarget {
        owner: identity.worker_id.clone(),
        runtime_incarnation: identity.lease_owner(),
        lease_expires_at_unix_ms: 10_020,
        reassign_existing_lease: false,
    };
    for (rule, mode, handed_out, retired) in [
        ("A1", WorkMode::NotRequired, true, false),
        ("A2", WorkMode::Free, true, false),
        ("A3 foreign", WorkMode::Foreign, false, true),
        ("A3 unowned", WorkMode::Unowned, false, true),
        ("A4", WorkMode::Unavailable, false, true),
    ] {
        *work.mode.lock().unwrap() = mode;
        let retirements_before = control.retirements.lock().unwrap().len();
        let claimed = lifecycle
            .claim_next_session_preparation(&identity, target.clone())
            .await;
        if mode == WorkMode::Unavailable {
            assert!(claimed.is_err(), "{rule} E4");
        } else {
            let assignment = claimed.unwrap_or_else(|error| panic!("{rule}: {error}"));
            assert_eq!(assignment.is_some(), handed_out, "{rule} E2/E3");
            if let Some(assignment) = assignment {
                assert_eq!(assignment.session_id.as_str(), "prepared", "{rule} E2");
            }
        }
        assert_eq!(
            work.causes.lock().unwrap().last(),
            Some(&SessionWorkAcquisition::ClaimedPreparation),
            "{rule} E1"
        );
        let retirements = control.retirements.lock().unwrap();
        assert_eq!(
            retirements.len(),
            retirements_before + usize::from(retired),
            "{rule} E3"
        );
        if retired {
            let retirement = retirements.last().expect("retired grant");
            assert_eq!(retirement.session_id.as_str(), "prepared", "{rule} E3");
            assert_eq!(retirement.lease.epoch, 3, "{rule} E3 the issued grant");
            assert_eq!(
                retirement.lease.runtime_incarnation,
                identity.lease_owner(),
                "{rule} E3 the issued grant"
            );
        }
    }
    assert_eq!(preparation.claims.load(Ordering::SeqCst), 5);
    assert_eq!(
        work.causes.lock().unwrap().len(),
        5,
        "one Work read per claim"
    );
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

/// Reuse the original selector while refusing only the final handoff validation.
struct FailingPreparationValidation {
    issuer: IssuingPreparation,
    failure: Mutex<awaken_session_execution_contract::SessionRealizationControlFailure>,
    validations: AtomicUsize,
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionPreparationControl for FailingPreparationValidation {
    async fn claim_next_session_preparation(
        &self,
        target: awaken_session_execution_contract::SessionRealizationTarget,
    ) -> Result<
        Option<awaken_session_execution_contract::SessionPreparationAssignment>,
        awaken_session_execution_contract::SessionRealizationControlFailure,
    > {
        awaken_session_execution_contract::SessionPreparationControl::claim_next_session_preparation(
            &self.issuer,
            target,
        )
        .await
    }

    async fn validate_session_preparation_assignment(
        &self,
        _assignment: &awaken_session_execution_contract::SessionPreparationAssignment,
        _target: &awaken_session_execution_contract::SessionRealizationTarget,
    ) -> Result<bool, awaken_session_execution_contract::SessionRealizationControlFailure> {
        self.validations.fetch_add(1, Ordering::SeqCst);
        Err(self.failure.lock().unwrap().clone())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn preparation_handoff_error_survives_exact_retirement_and_work_errors_keep_their_priority() {
    use awaken_session_contract::SessionRealizationRetirement as R;
    use awaken_session_execution_contract::SessionRealizationControlFailure as C;

    let clock = Arc::new(ManualClock::new(10_000));
    let credential = WorkerSigningCredential::new(
        "preparing-worker",
        "rotation",
        "credential",
        b"preparation-fixture-secret".to_vec(),
    )
    .unwrap();
    let authenticator: Arc<dyn WorkerRequestAuthenticator> = Arc::new(
        SignedWorkerAuthenticator::new(credential.clone(), clock.clone())
            .with_time_policy(60_000, 0),
    );
    let control = Arc::new(RecordingSessionControl::default());
    let preparation = Arc::new(FailingPreparationValidation {
        issuer: IssuingPreparation {
            claims: AtomicUsize::new(0),
        },
        failure: Mutex::new(C::Unavailable("handoff unavailable".into())),
        validations: AtomicUsize::new(0),
    });
    let work = Arc::new(PreparationWork {
        mode: Mutex::new(WorkMode::NotRequired),
        causes: Mutex::new(Vec::new()),
    });
    let service = WorkerDispatchService::new(RegisteredDispatchDependencies {
        authenticator,
        realizations: control.clone(),
        session_preparation: preparation.clone(),
        session_work: work.clone(),
        ..worker_dispatch_dependencies(
            Arc::new(MemoryDispatchStore::new(clock.clone())),
            Arc::new(awaken_worker_registry::MemoryWorkerDirectory::new()),
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
        .with_worker_id("preparing-worker")
        .with_request_authorizer(Arc::new(SignedWorkerRequestAuthorizer::new(
            credential,
            clock.clone(),
        )));
    let identity = WorkerControlClient::new(bootstrap.clone())
        .register_classified("preparation-boot", current_dispatch_codec_manifest())
        .await
        .unwrap()
        .worker
        .snapshot
        .identity;
    let lifecycle = WorkerControlClient::new(bootstrap.with_worker_identity(identity.clone()));
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
    let target = awaken_session_execution_contract::SessionRealizationTarget {
        owner: identity.worker_id.clone(),
        runtime_incarnation: identity.lease_owner(),
        lease_expires_at_unix_ms: 10_020,
        reassign_existing_lease: false,
    };
    // Each signed request issues one grant. A final validation failure remains
    // authoritative after retirement succeeds, refuses the grant, or fails.
    // Work failures skip validation and retain the original retirement-first order.
    let unavailable = C::Unavailable("handoff unavailable".into());
    let internal = C::Internal("handoff durable state invalid".into());
    let retirement_failure = C::Internal("retirement durable state invalid".into());
    let work_failure = C::Unavailable("run failed: Work store is unavailable".into());
    for (rule, mode, validation_failure, retirement_outcome, expected) in [
        (
            "validation unavailable, retired",
            WorkMode::Free,
            unavailable.clone(),
            Ok(R::Retired {}),
            unavailable.clone(),
        ),
        (
            "validation internal, superseded",
            WorkMode::Free,
            internal.clone(),
            Ok(R::Superseded {}),
            internal.clone(),
        ),
        (
            "validation unavailable before retirement internal",
            WorkMode::Free,
            unavailable.clone(),
            Err(retirement_failure.clone()),
            unavailable.clone(),
        ),
        (
            "validation internal before retirement unavailable",
            WorkMode::Free,
            internal.clone(),
            Err(C::Unavailable("retirement unavailable".into())),
            internal.clone(),
        ),
        (
            "work unavailable after retirement",
            WorkMode::Unavailable,
            internal.clone(),
            Ok(R::Retired {}),
            work_failure,
        ),
        (
            "retirement internal before work unavailable",
            WorkMode::Unavailable,
            internal,
            Err(retirement_failure.clone()),
            retirement_failure,
        ),
    ] {
        *work.mode.lock().unwrap() = mode;
        *preparation.failure.lock().unwrap() = validation_failure;
        *control.retirement_outcome.lock().unwrap() = Some(retirement_outcome);
        let validations_before = preparation.validations.load(Ordering::SeqCst);
        let retirements_before = control.retirements.lock().unwrap().len();
        let error = lifecycle
            .claim_next_session_preparation(&identity, target.clone())
            .await
            .unwrap_err();
        assert_eq!(
            error, expected,
            "{rule}: exact typed cause over signed HTTP"
        );
        assert_eq!(
            preparation.validations.load(Ordering::SeqCst),
            validations_before + usize::from(mode == WorkMode::Free),
            "{rule}: validation runs only after this Work grant is admitted"
        );
        let retirements = control.retirements.lock().unwrap();
        assert_eq!(
            retirements.len(),
            retirements_before + 1,
            "{rule}: retirement is attempted despite the prior failure"
        );
        let retirement = retirements.last().expect("exact issued grant retirement");
        assert_eq!(retirement.session_id.as_str(), "prepared", "{rule}");
        assert_eq!(
            retirement.lease,
            awaken_session_configuration_contract::resource::SessionRealizationLease {
                owner: target.owner.clone(),
                runtime_incarnation: target.runtime_incarnation.clone(),
                epoch: 3,
                expires_at_unix_ms: target.lease_expires_at_unix_ms,
            },
            "{rule}: retirement uses every coordinate of the issued grant"
        );
    }
    assert_eq!(preparation.issuer.claims.load(Ordering::SeqCst), 6);
    assert_eq!(preparation.validations.load(Ordering::SeqCst), 4);
    assert_eq!(
        *work.causes.lock().unwrap(),
        vec![SessionWorkAcquisition::ClaimedPreparation; 6],
        "one original Work acquisition per signed claim"
    );
    assert_eq!(control.retirements.lock().unwrap().len(), 6);
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}
