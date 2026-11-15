// @formal evidence worker.secret_materialization_is_claim_fenced
//! The one Worker credential grant endpoint over real HTTP: a database-less
//! Worker's `HttpDispatchQueue` asks the Coordinator router for brokered model
//! grants under its dispatch claim and for an MCP bearer under its Session MCP
//! generation lease. Every kind is admitted by the same rule; the rows below
//! name each refusal cause (another Worker, a stale or expired holding, a grant
//! the holding does not hold, a missing issuer, a holding lost during issuance).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::credential::{
    AuthChallenge, CandidateFingerprint, Credential, CredentialAccess, CredentialExecutionPolicy,
    CredentialMaterialSource, CredentialRef, CredentialRefresher, CredentialUsage,
    ModelExposurePolicy, PlaintextBoundary, PlaintextHolder,
};
use awaken_coordinator_client::{HttpDispatchQueue, WorkerMcpCredentialRefresh};
use awaken_credential_contract::{
    BrokeredGrantIssuer, BrokeredGrantOperation, BrokeredGrantRefusal, BrokeredGrantTarget,
    BrokeredModelAccess, ClaimCredentialGrant, ClaimCredentialGrantMaterial, GrantedSecret,
};
use awaken_dispatch::{
    DispatchError, MemoryDispatchStore, RunClaim, RunDispatch, WorkerCredentialGrant,
    WorkerCredentialGrantMaterial, WorkerCredentialGrantTransport,
};
use awaken_dispatch_contract::{DispatchClaims as _, DispatchEnqueue as _};
use awaken_dispatch_http::{
    RegisteredDispatchDependencies, WorkerCredentialGrantIssuers, WorkerDispatchService,
    dispatch_transport_router_with_service,
};
use awaken_dispatch_testkit::worker_dispatch::worker_dispatch_dependencies;
use awaken_dispatch_testkit::worker_http::{
    current_dispatch_codec_manifest, fixed_identity, ready_workers_with_manifest,
};
use awaken_lease_testkit::ManualClock;
use awaken_runtime_contract::InferenceEndpoint;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ModelBinding, ResolvedModelCandidate, ResolvedSpec,
};
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_session_configuration_contract::mcp::{
    McpAttachmentDraft, McpAttachmentOrigin, McpCredentialGrantRefusal, McpCredentialRefresh,
    McpGenerationRef, McpRealizationClaim, SessionMcpAttachmentSet,
};
use awaken_session_execution_contract as session;
use awaken_worker_transport_security::FixedWorkerLeasePolicy;

fn brokered_candidate() -> ResolvedModelCandidate {
    ResolvedModelCandidate::try_brokered_provider(
        ModelBinding::parse("openai", "gpt-5-e2e", "native").expect("valid fixture binding"),
        "openai@1",
        "awaken-cloud:openai:open_ai_responses@1",
        "workspace",
        InferenceEndpoint {
            adapter_kind: "openai".into(),
            api_dialect: "open_ai_responses".into(),
            base_url: "https://gateway.invalid".into(),
            upstream_model: "gpt-5-e2e".into(),
            processing_placement: None,
        },
    )
    .expect("coherent brokered candidate")
}

fn brokered_dispatch(run: &str) -> RunDispatch {
    let activation = RunActivation::new(
        RunId(run.into()),
        ThreadId(format!("{run}-thread")),
        ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snap".into()),
            metadata: Default::default(),
            root_agent_id: AgentId("agent".into()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: CatalogFingerprint("fp".into()),
                instructions: String::new(),
                max_steps: Some(4),
                delegation_limits: Default::default(),
                model_binding: brokered_candidate(),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: Default::default(),
                tool_presentation: Default::default(),
            },
            fingerprint: CatalogFingerprint("fp".into()),
        },
        vec![Message::text(MessageId("u1".into()), Role::User, "go")],
    );
    let mut dispatch = RunDispatch::new(activation);
    dispatch.placement.runtime_protocol_version = awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION;
    dispatch.with_worker_codec_requirements()
}

/// The Coordinator's broker identity: records every call and answers from a
/// script; `lose_claim_on_acquire` expires the dispatch lease while the grant
/// is being issued.
#[derive(Default)]
struct RecordingBroker {
    calls: Mutex<Vec<String>>,
    refuse: Mutex<Option<BrokeredGrantRefusal>>,
    lose_claim_on_acquire: Mutex<Option<(Arc<awaken_lease_testkit::ManualClock>, u64)>>,
    /// The expiry the broker gives a grant; `None` issues one that ends a
    /// minute after the Coordinator's clock (10 ms), like the Cloud broker.
    expires_at: Mutex<Option<i64>>,
}

/// One minute after the fixture Coordinator clock, in Unix seconds.
const MINUTE_GRANT: i64 = 60;

#[async_trait]
impl BrokeredGrantIssuer for RecordingBroker {
    async fn acquire(
        &self,
        target: BrokeredGrantTarget,
        idempotency_key: String,
    ) -> Result<BrokeredModelAccess, BrokeredGrantRefusal> {
        let mut calls = self.calls.lock().unwrap();
        calls.push(format!(
            "acquire:{}:{}:{}:{}:{idempotency_key}",
            target.provider,
            target.original_model_id,
            target.native_protocol,
            target.local_run_correlation
        ));
        if let Some((clock, expiry)) = self.lose_claim_on_acquire.lock().unwrap().take() {
            clock.set(expiry);
        }
        if let Some(refusal) = self.refuse.lock().unwrap().clone() {
            return Err(refusal);
        }
        Ok(BrokeredModelAccess {
            grant_id: format!("grant-{}", calls.len()),
            gateway_base_url: "https://gateway.invalid/v1".into(),
            capability: GrantedSecret::new(format!("capability-{}", calls.len())),
            grant_expires_at: self.expires_at.lock().unwrap().unwrap_or(MINUTE_GRANT),
        })
    }

    async fn renew(&self, grant_id: &str) -> Result<BrokeredModelAccess, BrokeredGrantRefusal> {
        self.calls.lock().unwrap().push(format!("renew:{grant_id}"));
        Ok(BrokeredModelAccess {
            grant_id: grant_id.into(),
            gateway_base_url: "https://gateway.invalid/v1".into(),
            capability: GrantedSecret::new("renewed"),
            grant_expires_at: MINUTE_GRANT,
        })
    }

    async fn close(&self, grant_id: &str) -> Result<(), BrokeredGrantRefusal> {
        self.calls.lock().unwrap().push(format!("close:{grant_id}"));
        Ok(())
    }
}

struct Coordinator {
    url: String,
    dispatch_clock: Arc<awaken_lease_testkit::ManualClock>,
}

async fn serve(
    issuers: WorkerCredentialGrantIssuers,
    mcp_sessions: Option<Arc<dyn session::SessionMcpCredentialGrants>>,
) -> Coordinator {
    let dispatch_clock = Arc::new(awaken_lease_testkit::ManualClock::new(10));
    let mem = Arc::new(MemoryDispatchStore::new(dispatch_clock.clone()));
    let base = worker_dispatch_dependencies(
        mem,
        ready_workers_with_manifest(
            &["worker-1", "worker-2"],
            &current_dispatch_codec_manifest(),
        ),
        Arc::new(ManualClock::new(10)),
    );
    let service = Arc::new(WorkerDispatchService::new(RegisteredDispatchDependencies {
        lease_policy: Arc::new(FixedWorkerLeasePolicy::new(1_000)),
        credential_grants: issuers,
        mcp_credential_grants: mcp_sessions.unwrap_or(base.mcp_credential_grants.clone()),
        ..base
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, dispatch_transport_router_with_service(service))
            .await
            .unwrap();
    });
    Coordinator {
        url: format!("http://{addr}"),
        dispatch_clock,
    }
}

fn worker(coordinator: &Coordinator, worker_id: &str) -> Arc<HttpDispatchQueue> {
    Arc::new(
        HttpDispatchQueue::new(coordinator.url.clone(), fixed_identity(worker_id))
            .with_worker_manifest(current_dispatch_codec_manifest()),
    )
}

async fn brokered(
    transport: &dyn WorkerCredentialGrantTransport,
    claim: &RunClaim,
    candidate: &CandidateFingerprint,
    operation: BrokeredGrantOperation,
) -> Result<Option<ClaimCredentialGrantMaterial>, DispatchError> {
    Ok(transport
        .credential_grant(WorkerCredentialGrant::Claim {
            claim: claim.clone(),
            grant: ClaimCredentialGrant::BrokeredModel {
                candidate_fingerprint: candidate.clone(),
                operation,
            },
        })
        .await?
        .map(|material| match material {
            WorkerCredentialGrantMaterial::Claim(material) => material,
            other => panic!("a claim grant answers claim material: {other:?}"),
        }))
}

fn acquire(key: &str) -> BrokeredGrantOperation {
    BrokeredGrantOperation::Acquire {
        idempotency_key: key.into(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn brokered_grants_are_admitted_only_to_the_live_claim_that_holds_the_candidate() {
    // Rule rows (the endpoint's one admission table, per cause):
    // B1 holder acquires for its frozen brokered candidate -> grant, with the
    //    target the Coordinator derived from the frozen candidate and the Run;
    // B2 the holder renews and closes the grant it was issued;
    // B3 renew/close of a grant this claim was not issued -> GrantNotHeld (400);
    // B4 a candidate the claim does not hold -> GrantNotHeld (400);
    // B5 another registered Worker presents the claim -> ForeignHolder (400);
    // B6 a stale epoch of the same Run -> no material (null);
    // B7 the broker's own refusal is relayed unchanged;
    // B8 the claim expires while the grant is issued -> null, grant closed;
    // B9 an expired lease -> no material; B10 no broker issuer -> the typed
    // cloud_models_disabled refusal (terminal, never a transport failure).
    let broker = Arc::new(RecordingBroker::default());
    let coordinator = serve(
        WorkerCredentialGrantIssuers {
            brokered: Some(broker.clone()),
            ..Default::default()
        },
        None,
    )
    .await;
    let holder = worker(&coordinator, "worker-1");
    holder
        .enqueue(brokered_dispatch("brokered-run"))
        .await
        .unwrap();
    let claimed = holder
        .claim("ignored", 30_000, &Default::default())
        .await
        .unwrap()
        .expect("the brokered Run is claimed");
    let claim = RunClaim::from(&claimed.lease);
    let candidate =
        awaken_credential_contract::candidate_fingerprint(&brokered_candidate()).unwrap();

    let Some(ClaimCredentialGrantMaterial::BrokeredModel(access)) =
        brokered(holder.as_ref(), &claim, &candidate, acquire("awaken-op-1"))
            .await
            .expect("B1 transport")
    else {
        panic!("B1 the holder receives a grant");
    };
    assert_eq!(
        broker.calls.lock().unwrap().as_slice(),
        ["acquire:openai:gpt-5-e2e:openai_responses:brokered-run:awaken-op-1"],
        "B1 exact target from the frozen candidate and the claimed Run"
    );
    assert_eq!(
        access.capability.into_redacted().expose_secret(),
        "capability-1",
        "B1"
    );
    let grant_id = access.grant_id;
    assert!(matches!(
        brokered(
            holder.as_ref(),
            &claim,
            &candidate,
            BrokeredGrantOperation::Renew {
                grant_id: grant_id.clone()
            }
        )
        .await,
        Ok(Some(ClaimCredentialGrantMaterial::BrokeredModel(_)))
    ));
    assert_eq!(
        brokered(
            holder.as_ref(),
            &claim,
            &candidate,
            BrokeredGrantOperation::Close {
                grant_id: grant_id.clone()
            }
        )
        .await
        .expect("B2 close"),
        Some(ClaimCredentialGrantMaterial::BrokeredModelClosed),
        "B2"
    );
    for (rule, operation) in [
        (
            "B3 closed grant",
            BrokeredGrantOperation::Close {
                grant_id: grant_id.clone(),
            },
        ),
        (
            "B3 unknown grant",
            BrokeredGrantOperation::Renew {
                grant_id: "grant-of-another-claim".into(),
            },
        ),
    ] {
        assert!(
            brokered(holder.as_ref(), &claim, &candidate, operation)
                .await
                .is_err(),
            "{rule}"
        );
    }
    assert!(
        brokered(
            holder.as_ref(),
            &claim,
            &CandidateFingerprint("not-frozen".into()),
            acquire("k")
        )
        .await
        .is_err(),
        "B4"
    );
    let other = worker(&coordinator, "worker-2");
    assert!(
        brokered(other.as_ref(), &claim, &candidate, acquire("k"))
            .await
            .is_err(),
        "B5 another Worker cannot use the claim"
    );
    let stale = RunClaim {
        epoch: awaken_lease_contract::fence::Epoch::from_persisted(claim.epoch)
            .and_then(awaken_lease_contract::fence::Epoch::next)
            .expect("a successor epoch")
            .get(),
        ..claim.clone()
    };
    assert_eq!(
        brokered(holder.as_ref(), &stale, &candidate, acquire("k"))
            .await
            .expect("B6 transport"),
        None,
        "B6"
    );
    *broker.refuse.lock().unwrap() = Some(BrokeredGrantRefusal::QuotaExceeded {
        retry_after_ms: Some(7_000),
    });
    assert_eq!(
        brokered(holder.as_ref(), &claim, &candidate, acquire("k"))
            .await
            .expect("B7 transport"),
        Some(ClaimCredentialGrantMaterial::BrokeredModelRefused(
            BrokeredGrantRefusal::QuotaExceeded {
                retry_after_ms: Some(7_000)
            }
        )),
        "B7"
    );
    *broker.refuse.lock().unwrap() = None;
    *broker.lose_claim_on_acquire.lock().unwrap() = Some((
        coordinator.dispatch_clock.clone(),
        claimed.lease.expires_ms(),
    ));
    let before = broker.calls.lock().unwrap().len();
    assert_eq!(
        brokered(holder.as_ref(), &claim, &candidate, acquire("k"))
            .await
            .expect("B8 transport"),
        None,
        "B8 a claim lost during issuance receives nothing"
    );
    let calls = broker.calls.lock().unwrap()[before..].to_vec();
    assert!(
        calls.len() == 2 && calls[0].starts_with("acquire:") && calls[1].starts_with("close:"),
        "B8 the issued grant is closed at once: {calls:?}"
    );
    assert_eq!(
        brokered(holder.as_ref(), &claim, &candidate, acquire("k"))
            .await
            .expect("B9 transport"),
        None,
        "B9 an expired lease holds nothing"
    );

    let bare = serve(WorkerCredentialGrantIssuers::default(), None).await;
    let holder = worker(&bare, "worker-1");
    holder.enqueue(brokered_dispatch("bare-run")).await.unwrap();
    let claimed = holder
        .claim("ignored", 30_000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        brokered(
            holder.as_ref(),
            &RunClaim::from(&claimed.lease),
            &candidate,
            acquire("k"),
        )
        .await
        .expect("B10 a typed answer, not a transport failure"),
        Some(ClaimCredentialGrantMaterial::BrokeredModelRefused(
            BrokeredGrantRefusal::CloudModelsDisabled
        )),
        "B10 a Coordinator without a broker supplies no brokered models"
    );
}

/// Two Coordinator processes over one Dispatch store and one broker: the
/// second is the first after a restart (empty issue ledger).
async fn serve_on(mem: Arc<MemoryDispatchStore>, broker: Arc<RecordingBroker>) -> String {
    let service = Arc::new(WorkerDispatchService::new(RegisteredDispatchDependencies {
        lease_policy: Arc::new(FixedWorkerLeasePolicy::new(1_000)),
        credential_grants: WorkerCredentialGrantIssuers {
            brokered: Some(broker),
            ..Default::default()
        },
        ..worker_dispatch_dependencies(
            mem,
            ready_workers_with_manifest(&["worker-1"], &current_dispatch_codec_manifest()),
            Arc::new(ManualClock::new(10)),
        )
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, dispatch_transport_router_with_service(service))
            .await
            .unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_close_refused_after_a_restart_leaves_only_a_grant_that_ends_on_its_own() {
    // The issue ledger is process memory. B11 the broker issues a grant that
    // outlives the bound -> it is released at once and never returned, so
    // every returned grant ends on the broker's clock within the bound.
    // B12 after a Coordinator restart, close/renew of a grant issued before is
    // refused (GrantNotHeld) and the broker is never asked to close or extend
    // anything: no other claim's grant can be closed, and the refused grant is
    // the bounded one B11 admitted, held only by its own attempt.
    let dispatch_clock = Arc::new(awaken_lease_testkit::ManualClock::new(10));
    let mem = Arc::new(MemoryDispatchStore::new(dispatch_clock));
    let broker = Arc::new(RecordingBroker::default());
    let before_restart = serve_on(mem.clone(), broker.clone()).await;
    let holder = Arc::new(
        HttpDispatchQueue::new(before_restart, fixed_identity("worker-1"))
            .with_worker_manifest(current_dispatch_codec_manifest()),
    );
    holder
        .enqueue(brokered_dispatch("restart-run"))
        .await
        .unwrap();
    let claimed = holder
        .claim("ignored", 30_000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let claim = RunClaim::from(&claimed.lease);
    let candidate =
        awaken_credential_contract::candidate_fingerprint(&brokered_candidate()).unwrap();

    *broker.expires_at.lock().unwrap() = Some(i64::MAX / 2);
    assert_eq!(
        brokered(holder.as_ref(), &claim, &candidate, acquire("unbounded"))
            .await
            .expect("B11 transport"),
        Some(ClaimCredentialGrantMaterial::BrokeredModelRefused(
            BrokeredGrantRefusal::TemporarilyUnavailable
        )),
        "B11 an unbounded grant is never returned"
    );
    assert_eq!(
        broker.calls.lock().unwrap().last().map(String::as_str),
        Some("close:grant-1"),
        "B11 it is released at once"
    );
    *broker.expires_at.lock().unwrap() = None;
    let Some(ClaimCredentialGrantMaterial::BrokeredModel(access)) =
        brokered(holder.as_ref(), &claim, &candidate, acquire("bounded"))
            .await
            .expect("B11 transport")
    else {
        panic!("B11 a bounded grant is returned");
    };
    assert!(
        access.grant_expires_at <= MINUTE_GRANT,
        "B11 bounded expiry"
    );

    let after_restart = serve_on(mem, broker.clone()).await;
    let restarted = Arc::new(
        HttpDispatchQueue::new(after_restart, fixed_identity("worker-1"))
            .with_worker_manifest(current_dispatch_codec_manifest()),
    );
    let calls_before = broker.calls.lock().unwrap().len();
    for operation in [
        BrokeredGrantOperation::Close {
            grant_id: access.grant_id.clone(),
        },
        BrokeredGrantOperation::Renew {
            grant_id: access.grant_id.clone(),
        },
    ] {
        assert!(
            brokered(restarted.as_ref(), &claim, &candidate, operation)
                .await
                .is_err(),
            "B12 a grant this process did not issue is not held"
        );
    }
    assert_eq!(
        broker.calls.lock().unwrap().len(),
        calls_before,
        "B12 the broker is asked to close or extend nothing"
    );
    assert!(
        matches!(
            brokered(
                restarted.as_ref(),
                &claim,
                &candidate,
                acquire("after-restart")
            )
            .await,
            Ok(Some(ClaimCredentialGrantMaterial::BrokeredModel(_)))
        ),
        "B12 the live claim still acquires new grants after the restart"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provider_grant_is_refused_for_a_brokered_candidate() {
    // G2: a Provider delivery names a candidate whose access is brokered: the
    // claim holds no attempt binding for it, so no delivery source is asked.
    struct NeverDeliver;
    #[async_trait]
    impl awaken_credential_contract::AttemptCredentialDeliverySource for NeverDeliver {
        async fn deliver(
            &self,
            _: awaken_credential_contract::AttemptCredentialDeliveryRequest,
        ) -> Result<awaken_credential_contract::AttemptCredentialDelivery, String> {
            panic!("G2 no delivery for an unbound candidate")
        }
    }
    let coordinator = serve(
        WorkerCredentialGrantIssuers {
            provider: Some(Arc::new(NeverDeliver)),
            ..Default::default()
        },
        None,
    )
    .await;
    let holder = worker(&coordinator, "worker-1");
    holder
        .enqueue(brokered_dispatch("provider-run"))
        .await
        .unwrap();
    let claimed = holder
        .claim("ignored", 30_000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let candidate =
        awaken_credential_contract::candidate_fingerprint(&brokered_candidate()).unwrap();
    let refused = holder
        .credential_grant(WorkerCredentialGrant::Claim {
            claim: RunClaim::from(&claimed.lease),
            grant: ClaimCredentialGrant::ProviderCredential {
                candidate_fingerprint: candidate,
            },
        })
        .await;
    assert!(refused.is_err(), "G2: {refused:?}");
}

/// The Session read of the endpoint over one fixed MCP attachment set: the
/// aggregate's own grant law decides.
struct McpGrantSessions {
    set: SessionMcpAttachmentSet,
    now: u64,
}

#[async_trait]
impl session::SessionMcpCredentialGrants for McpGrantSessions {
    async fn mcp_credential_grant(
        &self,
        generation: &McpGenerationRef,
        credential: &CredentialRef,
    ) -> Result<
        Result<CredentialAccess, McpCredentialGrantRefusal>,
        session::SessionRealizationControlFailure,
    > {
        Ok(self
            .set
            .credential_grant(generation, credential, self.now)
            .cloned())
    }
}

/// The Coordinator's Vault authority stand-in: it records the pin it was
/// asked to refresh and answers with a fresh bearer.
#[derive(Default)]
struct RecordingVault(Mutex<Vec<String>>);

struct FreshBearer;

#[async_trait]
impl CredentialRefresher for FreshBearer {
    async fn refresh(&self, challenge: &AuthChallenge) -> Option<Credential> {
        (challenge.status == 401).then(|| Credential::Bearer("fresh-bearer".into()))
    }
}

impl McpCredentialRefresh for RecordingVault {
    fn refresher(
        &self,
        generation: &McpGenerationRef,
        credential: &CredentialAccess,
    ) -> Arc<dyn CredentialRefresher> {
        self.0.lock().unwrap().push(format!(
            "{}:{}@{}",
            generation.runtime_incarnation(),
            credential.credential.id,
            credential.credential.revision
        ));
        Arc::new(FreshBearer)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_mcp_bearer_is_refreshed_only_for_the_generation_lease_that_pins_it() {
    // M-rows over the same endpoint: R1 the holder of the current generation
    // lease refreshes its pinned bearer through the Coordinator's Vault
    // authority (Worker refresher end to end); R2 another Worker presenting
    // that lease -> ForeignHolder (no bearer, Vault untouched); R3 an older
    // lease epoch -> stale (no bearer); R4 a credential the generation does
    // not pin -> GrantNotHeld; R5 no MCP issuer -> the typed IssuerMissing answer.
    let holder = PlaintextHolder::new(PlaintextBoundary::Worker, "awaken.worker");
    let pinned = CredentialRef {
        id: "mcp-bearer".into(),
        revision: 3,
    };
    let mut set = SessionMcpAttachmentSet::from_initial(
        vec![McpAttachmentDraft {
            name: "docs".into(),
            target: awaken_agent_contract::McpTarget::parse_http("https://docs.example.test")
                .unwrap(),
            prompts_as_skills: false,
            credential: Some(CredentialAccess::new(
                pinned.clone(),
                CredentialMaterialSource::ControlPlaneReference,
                CredentialUsage::HttpHeader {
                    name: "authorization".into(),
                    scheme: Some("Bearer".into()),
                },
                CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::Forbidden),
            )),
            origin: McpAttachmentOrigin::Session,
        }],
        Some(holder),
    )
    .unwrap();
    let generation = set.attachments[0].generation.clone();
    let claim = McpRealizationClaim {
        realization_id: "realization".into(),
        runtime_incarnation: fixed_identity("worker-1").lease_owner(),
        lease_epoch: 4,
        lease_expires_at_unix_ms: 1_000_000,
        stage_idempotency_key: "stage".into(),
    };
    set.claim_realization(&generation, claim.clone()).unwrap();
    let lease = McpGenerationRef::from_claim(
        &awaken_session_types::SessionId::from("session"),
        &generation,
        &claim,
    )
    .unwrap();
    let set_for_r5 = set.clone();
    let vault = Arc::new(RecordingVault::default());
    let coordinator = serve(
        WorkerCredentialGrantIssuers {
            mcp: Some(vault.clone()),
            ..Default::default()
        },
        Some(Arc::new(McpGrantSessions { set, now: 10 })),
    )
    .await;
    let challenge = AuthChallenge {
        status: 401,
        www_authenticate: Some("Bearer realm=\"mcp\"".into()),
    };
    let pin = |credential: CredentialRef| {
        CredentialAccess::new(
            credential,
            CredentialMaterialSource::ControlPlaneReference,
            CredentialUsage::HttpHeader {
                name: "authorization".into(),
                scheme: Some("Bearer".into()),
            },
            CredentialExecutionPolicy::exact(
                PlaintextHolder::new(PlaintextBoundary::Worker, "awaken.worker"),
                ModelExposurePolicy::Forbidden,
            ),
        )
    };
    let refresh = |worker_id: &str, generation: &McpGenerationRef, credential: CredentialRef| {
        WorkerMcpCredentialRefresh::new(worker(&coordinator, worker_id))
            .refresher(generation, &pin(credential))
    };
    assert_eq!(
        refresh("worker-1", &lease, pinned.clone())
            .refresh(&challenge)
            .await,
        Some(Credential::Bearer("fresh-bearer".into())),
        "R1"
    );
    assert_eq!(
        vault.0.lock().unwrap().as_slice(),
        [format!("{}:mcp-bearer@3", lease.runtime_incarnation())],
        "R1 the Coordinator refreshed exactly the generation's pin"
    );
    let older = lease.with_persisted(|wire| wire.lease_epoch = 3).unwrap();
    for (rule, worker_id, generation, credential) in [
        ("R2", "worker-2", &lease, pinned.clone()),
        ("R3", "worker-1", &older, pinned.clone()),
        (
            "R4",
            "worker-1",
            &lease,
            CredentialRef {
                id: "mcp-bearer".into(),
                revision: 4,
            },
        ),
    ] {
        assert_eq!(
            refresh(worker_id, generation, credential)
                .refresh(&challenge)
                .await,
            None,
            "{rule}"
        );
    }
    assert_eq!(
        vault.0.lock().unwrap().len(),
        1,
        "R2-R4 never reach the Vault"
    );

    let without_vault = serve(
        WorkerCredentialGrantIssuers::default(),
        Some(Arc::new(McpGrantSessions {
            set: set_for_r5,
            now: 10,
        })),
    )
    .await;
    assert_eq!(
        worker(&without_vault, "worker-1")
            .credential_grant(WorkerCredentialGrant::McpGeneration {
                generation: lease,
                credential: pinned,
                challenge_status: 401,
                www_authenticate: None,
            })
            .await
            .expect("R5 a typed answer, not a transport failure"),
        Some(WorkerCredentialGrantMaterial::IssuerMissing(
            awaken_dispatch::CredentialGrantKind::McpBearer
        )),
        "R5 a Coordinator without the Vault authority says so"
    );
}
