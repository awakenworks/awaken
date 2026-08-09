use std::sync::Arc;

use awaken_dream_application::{
    BUILT_IN_DREAM_AGENT_ID, DreamApplication, DreamCancellation, DreamExecutor, DreamFailure,
    DreamModelReadiness, DreamPolicyConfig, DreamPreparation, DreamRequest, DreamSessionSource,
    InMemoryDreamProcessStore,
};
use awaken_protocol_managed::{DREAMING_BETA, dreams_router, enforce_managed_beta};
use awaken_session_contract::DreamProcessStore;
use awaken_session_contract::{DreamModelConfig, DreamUsage};
use awaken_session_store::SqliteManagedSessionRepository;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tower::ServiceExt;

#[derive(Clone, Copy)]
enum Outcome {
    Complete,
    Fail,
    Block,
}

fn in_memory_application(executor: Arc<dyn DreamExecutor>) -> DreamApplication {
    DreamApplication::with_store(executor, Arc::new(InMemoryDreamProcessStore::default()))
        .expect("empty test Dream store")
}

struct FixedSessionFacts;

#[async_trait::async_trait]
impl DreamSessionSource for FixedSessionFacts {
    async fn eligible_sessions(
        &self,
        _workspace_id: &str,
        _updated_after_ms: u64,
        _limit: usize,
    ) -> Vec<String> {
        Vec::new()
    }

    fn session_usage(&self, _workspace_id: &str, _session_id: &str) -> Option<DreamUsage> {
        Some(DreamUsage {
            input_tokens: 12,
            output_tokens: 3,
            ..Default::default()
        })
    }
}

struct AlternateSessionFacts;

#[async_trait::async_trait]
impl DreamSessionSource for AlternateSessionFacts {
    async fn eligible_sessions(
        &self,
        _workspace_id: &str,
        _updated_after_ms: u64,
        _limit: usize,
    ) -> Vec<String> {
        Vec::new()
    }

    fn session_usage(&self, _workspace_id: &str, _session_id: &str) -> Option<DreamUsage> {
        Some(DreamUsage {
            input_tokens: 99,
            output_tokens: 8,
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn automatic_policy_is_opt_in_thresholded_and_reuses_the_dream_job_path() {
    struct Sessions(Arc<std::sync::atomic::AtomicBool>);
    #[async_trait::async_trait]
    impl DreamSessionSource for Sessions {
        async fn eligible_sessions(
            &self,
            _workspace_id: &str,
            updated_after_ms: u64,
            limit: usize,
        ) -> Vec<String> {
            if updated_after_ms > 0 {
                Vec::new()
            } else if self.0.load(std::sync::atomic::Ordering::SeqCst) {
                vec!["s1".into()]
            } else {
                ["s1", "s2", "s3"]
                    .into_iter()
                    .take(limit)
                    .map(str::to_string)
                    .collect()
            }
        }
    }

    // Policy decision table:
    // P1 absent/default-disabled -> no automatic Dream;
    // P2 enabled but fewer than min -> advance schedule, no job;
    // P3 enabled + threshold -> exactly one ordinary DreamProcess using max bound;
    // P4 successful job advances cutoff -> unchanged Sessions are not reprocessed.
    let (state, _, _) = state(Outcome::Complete);
    let sparse = Arc::new(std::sync::atomic::AtomicBool::new(true));
    state.bind_session_source(Arc::new(Sessions(sparse.clone())));
    state
        .set_policy(
            "default",
            "mem_policy",
            DreamPolicyConfig {
                enabled: false,
                interval_seconds: 60,
                min_new_sessions: 2,
                max_sessions: 2,
                model: DreamModelConfig {
                    id: "claude-sonnet-5".into(),
                    speed: None,
                },
                instructions: Some("Keep durable facts.".into()),
            },
        )
        .unwrap();
    assert!(
        state.tick_policies(u64::MAX).await.unwrap().is_empty(),
        "P1"
    );
    state
        .set_policy(
            "default",
            "mem_policy",
            DreamPolicyConfig {
                enabled: true,
                interval_seconds: 60,
                min_new_sessions: 2,
                max_sessions: 2,
                model: DreamModelConfig {
                    id: "claude-sonnet-5".into(),
                    speed: None,
                },
                instructions: Some("Keep durable facts.".into()),
            },
        )
        .unwrap();
    assert!(
        state.tick_policies(u64::MAX).await.unwrap().is_empty(),
        "P2"
    );
    sparse.store(false, std::sync::atomic::Ordering::SeqCst);
    let created = state.tick_policies(u64::MAX).await.unwrap();
    assert_eq!(created.len(), 1, "P3");
    assert_eq!(
        created[0].inputs[1],
        serde_json::from_value(json!({
            "type":"sessions", "session_ids":["s1", "s2"]
        }))
        .unwrap(),
        "P3 max bound"
    );
    let app = dreams_router(state.clone());
    wait_for_status(&app, &created[0].id, "completed").await;
    assert!(
        state.tick_policies(u64::MAX).await.unwrap().is_empty(),
        "P4"
    );
}

#[tokio::test]
async fn dream_policy_application_projects_defaults_validates_and_survives_restart() {
    // Coordinator policy cause/effect decision table:
    // A1 no durable row -> disabled effective default and no cursor;
    // A2 invalid interval/session/model bounds -> reject and no row;
    // A3 valid command -> configured projection with a due cursor;
    // A4 process restart -> the same Workspace/MemoryStore policy is restored.
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "awaken-dream-policy-{}-{unique}.db",
        std::process::id()
    ));
    let repository =
        Arc::new(SqliteManagedSessionRepository::open(path.to_str().unwrap()).unwrap());
    let worker = Arc::new(Worker {
        outcome: Outcome::Complete,
        started: Arc::new(Notify::new()),
        release: Arc::new(Notify::new()),
    });
    let state = Arc::new(DreamApplication::with_store(worker, repository).unwrap());
    let default = state.policy("default", "mem_policy").unwrap();
    assert!(!default.config.enabled, "A1");
    assert!(default.next_due_at.is_none(), "A1");
    assert!(
        state
            .set_policy(
                "default",
                "mem_policy",
                DreamPolicyConfig {
                    enabled: true,
                    interval_seconds: 59,
                    min_new_sessions: 2,
                    max_sessions: 10,
                    model: DreamModelConfig {
                        id: "claude-sonnet-5".into(),
                        speed: None
                    },
                    instructions: None,
                }
            )
            .is_err(),
        "A2"
    );
    state
        .set_policy(
            "default",
            "mem_policy",
            DreamPolicyConfig {
                enabled: true,
                interval_seconds: 3600,
                min_new_sessions: 2,
                max_sessions: 10,
                model: DreamModelConfig {
                    id: "claude-sonnet-5".into(),
                    speed: None,
                },
                instructions: Some("Keep durable decisions.".into()),
            },
        )
        .unwrap();
    let configured = state.policy("default", "mem_policy").unwrap();
    assert!(configured.config.enabled, "A3");
    assert!(configured.next_due_at.is_some(), "A3");
    drop(state);

    let reopened = Arc::new(
        DreamApplication::with_store(
            Arc::new(Worker {
                outcome: Outcome::Complete,
                started: Arc::new(Notify::new()),
                release: Arc::new(Notify::new()),
            }),
            Arc::new(SqliteManagedSessionRepository::open(path.to_str().unwrap()).unwrap()),
        )
        .unwrap(),
    );
    let restored = reopened.policy("default", "mem_policy").unwrap();
    assert!(restored.config.enabled, "A4");
    assert_eq!(restored.config.interval_seconds, 3600, "A4");
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn concurrent_policy_ticks_claim_one_dream_across_replicas() {
    struct Sessions;
    #[async_trait::async_trait]
    impl DreamSessionSource for Sessions {
        async fn eligible_sessions(
            &self,
            _workspace_id: &str,
            _updated_after_ms: u64,
            _limit: usize,
        ) -> Vec<String> {
            vec!["s1".into()]
        }
    }
    let repository = Arc::new(SqliteManagedSessionRepository::open_in_memory().unwrap());
    let make_state = || {
        Arc::new(
            DreamApplication::with_store(
                Arc::new(Worker {
                    outcome: Outcome::Complete,
                    started: Arc::new(Notify::new()),
                    release: Arc::new(Notify::new()),
                }),
                repository.clone(),
            )
            .unwrap(),
        )
    };
    let first = make_state();
    let second = make_state();
    first
        .set_policy(
            "default",
            "mem",
            DreamPolicyConfig {
                enabled: true,
                interval_seconds: 60,
                min_new_sessions: 1,
                max_sessions: 1,
                model: DreamModelConfig {
                    id: "claude-sonnet-5".into(),
                    speed: None,
                },
                instructions: None,
            },
        )
        .unwrap();
    first.bind_session_source(Arc::new(Sessions));
    second.bind_session_source(Arc::new(Sessions));

    // Replica decision rule: C0 both applications are constructed before the
    // policy write -> both must read the shared store rather than a local mirror;
    // C1 two schedulers hold the same due policy version
    // and both find eligible evidence -> E1 exact-version policy+job transaction
    // accepts one claimant, E2 the loser is a benign no-op, E3 one durable job.
    let (left, right) = tokio::join!(
        first.tick_policies(u64::MAX),
        second.tick_policies(u64::MAX)
    );
    assert_eq!(left.unwrap().len() + right.unwrap().len(), 1);
    assert_eq!(repository.dream_processes().unwrap().len(), 1);
}

struct Worker {
    outcome: Outcome,
    started: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait::async_trait]
impl DreamExecutor for Worker {
    async fn validate_inputs(&self, request: &DreamRequest) -> Result<(), DreamFailure> {
        if request.source_memory_store_id == "missing" {
            Err(DreamFailure::new(
                "input_memory_store_unavailable",
                "missing input",
            ))
        } else {
            Ok(())
        }
    }

    async fn prepare(&self, request: &DreamRequest) -> Result<DreamPreparation, DreamFailure> {
        Ok(DreamPreparation {
            result_memory_store_id: format!("mem_result_{}", request.job_id),
            session_id: format!("sesn_{}", request.job_id),
            transcript_file_ids: Vec::new(),
        })
    }

    async fn execute(
        &self,
        _request: &DreamRequest,
        _preparation: &DreamPreparation,
        _cancellation: DreamCancellation,
    ) -> Result<(), DreamFailure> {
        self.started.notify_waiters();
        match self.outcome {
            Outcome::Complete => Ok(()),
            Outcome::Fail => Err(DreamFailure::new("internal_error", "planned failure")),
            Outcome::Block => {
                self.release.notified().await;
                Ok(())
            }
        }
    }
}

fn state(outcome: Outcome) -> (Arc<DreamApplication>, Arc<Notify>, Arc<Notify>) {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let worker = Arc::new(Worker {
        outcome,
        started: started.clone(),
        release: release.clone(),
    });
    let state = Arc::new(in_memory_application(worker));
    state.bind_session_source(Arc::new(FixedSessionFacts));
    (state, started, release)
}

fn create_body(memory: &str, sessions: &[&str]) -> Value {
    json!({
        "inputs": [
            {"type":"memory_store", "memory_store_id":memory},
            {"type":"sessions", "session_ids":sessions},
        ],
        "model": {"id":"claude-sonnet-5", "speed":"standard"},
        "instructions":"Prefer durable decisions."
    })
}

struct NoReadyDreamModels;

#[async_trait::async_trait]
impl DreamModelReadiness for NoReadyDreamModels {
    async fn is_ready(&self, _workspace_id: &str, _model_id: &str) -> Result<bool, String> {
        Ok(false)
    }
}

#[tokio::test]
async fn create_rejects_a_supported_but_not_executable_workspace_model() {
    let (state, _, _) = state(Outcome::Complete);
    state.bind_model_readiness(Arc::new(NoReadyDreamModels));
    let app = dreams_router(state);
    let (status, body) = request(
        &app,
        "POST",
        "/v1/dreams",
        Some(create_body("memory", &["session"])),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not connected or executable")
    );
}

async fn request(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let response = app
        .clone()
        .oneshot(
            builder
                .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

async fn wait_for_status(app: &axum::Router, id: &str, expected: &str) -> Value {
    for _ in 0..100 {
        let (_, value) = request(app, "GET", &format!("/v1/dreams/{id}"), None).await;
        if value["status"] == expected {
            return value;
        }
        tokio::task::yield_now().await;
    }
    panic!("Dream did not reach {expected}")
}

#[tokio::test]
async fn official_create_retrieve_list_archive_and_failure_shapes() {
    // Cause/effect graph and decision rules (Anthropic Dreams contract):
    // C1 valid memory + 1..=100 Sessions + model -> E1 async Dream; D1.
    // C2 worker completes -> E2 output/session/usage retained; D2.
    // C3 terminal archive -> E3 timestamp set, status unchanged, default list hides; D3.
    // C4 worker fails after prepare -> E4 failed + typed error + partial output retained; D4.
    let (complete, _, _) = state(Outcome::Complete);
    let app = dreams_router(complete);
    let (status, created) = request(
        &app,
        "POST",
        "/v1/dreams?beta=true",
        Some(create_body("mem_1", &["sesn_1"])),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(created["type"], "dream");
    assert_eq!(created["status"], "pending");
    let id = created["id"].as_str().unwrap();
    let completed = wait_for_status(&app, id, "completed").await;
    assert_eq!(completed["outputs"][0]["type"], "memory_store");
    assert_eq!(completed["session_id"], format!("sesn_{id}"));
    assert_eq!(completed["usage"]["input_tokens"], 12);

    let (status, archived) = request(&app, "POST", &format!("/v1/dreams/{id}/archive"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(archived["status"], "completed");
    assert!(archived["archived_at"].is_string());
    let (_, default_list) = request(&app, "GET", "/v1/dreams", None).await;
    assert_eq!(default_list["data"].as_array().unwrap().len(), 0);
    let (_, archived_list) = request(&app, "GET", "/v1/dreams?include_archived=true", None).await;
    assert_eq!(archived_list["data"].as_array().unwrap().len(), 1);

    let (failed_state, _, _) = state(Outcome::Fail);
    let failed_app = dreams_router(failed_state);
    let (_, created) = request(
        &failed_app,
        "POST",
        "/v1/dreams",
        Some(create_body("mem_1", &["sesn_1"])),
    )
    .await;
    let failed = wait_for_status(&failed_app, created["id"].as_str().unwrap(), "failed").await;
    assert_eq!(failed["error"]["type"], "internal_error");
    assert_eq!(failed["outputs"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn validation_and_terminal_mutation_decision_table() {
    // Causes: input-union cardinality, Session count/uniqueness, instruction/model
    // bounds, input availability, and lifecycle state. Effects/decision rules:
    // D1 each invalid create cause -> 400 and no job; D2 missing input -> 400;
    // D3 archive(nonterminal) -> 400; D4 cancel(completed|failed) -> 400.
    let (state, _, _) = state(Outcome::Complete);
    let app = dreams_router(state);
    let invalid = [
        json!({"inputs":[], "model":"m"}),
        create_body("mem", &[]),
        create_body("mem", &["same", "same"]),
        json!({
            "inputs":[{"type":"memory_store","memory_store_id":"mem"},{"type":"sessions","session_ids":["s"]}],
            "model":""
        }),
        json!({
            "inputs":[{"type":"memory_store","memory_store_id":"mem"},{"type":"sessions","session_ids":["s"]}],
            "model":"m", "instructions":"x".repeat(4097)
        }),
        create_body("missing", &["s"]),
        json!({
            "inputs":[{"type":"memory_store","memory_store_id":"mem"},{"type":"sessions","session_ids":["s"]}],
            "model":{"id":"claude-sonnet-5","speed":"fast"}
        }),
    ];
    for body in invalid {
        let (status, _) = request(&app, "POST", "/v1/dreams", Some(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    let (_, created) = request(&app, "POST", "/v1/dreams", Some(create_body("mem", &["s"]))).await;
    let id = created["id"].as_str().unwrap();
    let _ = wait_for_status(&app, id, "completed").await;
    let (status, _) = request(&app, "POST", &format!("/v1/dreams/{id}/cancel"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn cancellation_is_immediate_idempotent_and_retains_prepared_output() {
    // C1 running prepared job + cancel -> E1 public status immediately canceled;
    // C2 output already cloned -> E2 retained; C3 repeated cancel -> E3 same object;
    // C4 late worker completion -> E4 cannot overwrite canceled. Rules D1-D4.
    let (state, started, release) = state(Outcome::Block);
    let app = dreams_router(state);
    let (_, created) = request(&app, "POST", "/v1/dreams", Some(create_body("mem", &["s"]))).await;
    let id = created["id"].as_str().unwrap();
    started.notified().await;
    let (status, canceled) = request(&app, "POST", &format!("/v1/dreams/{id}/cancel"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(canceled["status"], "canceled");
    assert_eq!(canceled["outputs"].as_array().unwrap().len(), 1);
    let (_, repeated) = request(&app, "POST", &format!("/v1/dreams/{id}/cancel"), None).await;
    assert_eq!(repeated["status"], "canceled");
    release.notify_waiters();
    tokio::task::yield_now().await;
    let (_, retrieved) = request(&app, "GET", &format!("/v1/dreams/{id}"), None).await;
    assert_eq!(retrieved["status"], "canceled");
}

#[tokio::test]
async fn dream_routes_require_managed_and_dreaming_betas() {
    // Header causes/effects: neither/one beta -> 400; both capabilities -> route.
    // This is the four-rule truth table for the overview's Managed beta plus the
    // Dreams research-preview beta; query `beta=true` never substitutes for headers.
    let (state, _, _) = state(Outcome::Complete);
    let app = dreams_router(state).layer(axum::middleware::from_fn(enforce_managed_beta));
    for path in ["/v1/dreams?beta=true"] {
        for header in [
            None,
            Some(awaken_managed_bridge::MANAGED_BETA),
            Some(DREAMING_BETA),
        ] {
            let mut builder = Request::builder().method("GET").uri(path);
            if let Some(header) = header {
                builder = builder.header("anthropic-beta", header);
            }
            let response = app
                .clone()
                .oneshot(builder.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
    }
    for path in ["/v1/dreams?beta=true"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(path)
                    .header(
                        "anthropic-beta",
                        format!("{},{}", awaken_managed_bridge::MANAGED_BETA, DREAMING_BETA),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}

#[tokio::test]
async fn compatible_dream_router_has_no_awaken_only_configuration_routes() {
    // Cause/effect decision table: R1 official Dream route -> owned by the
    // compatible router; R2 former Dream policy API and R3 former dedicated
    // Dream-Agent API -> 404. Auxiliary Agent authoring uses ordinary Agent
    // configuration, so neither removed path may regain an owner.
    let (state, _, _) = state(Outcome::Complete);
    let app = dreams_router(state);
    assert_ne!(
        request(&app, "GET", "/v1/dreams", None).await.0,
        StatusCode::NOT_FOUND,
        "R1"
    );
    for path in ["/v1/dream_policies/mem_1", "/v1/dream_agent_configuration"] {
        assert_eq!(
            request(&app, "GET", path, None).await.0,
            StatusCode::NOT_FOUND,
            "removed extension {path}"
        );
    }
}

#[tokio::test]
async fn list_supports_official_repeated_status_filters_and_cursor_pages() {
    // List causes/effects: newest-first inventory + limit -> E1 page cursor;
    // repeated statuses -> E2 OR filter; invalid bound/limit -> E3 400. Rules
    // L1-L3 cover the SDK PageCursor and DreamListParams contract.
    let (state, _, _) = state(Outcome::Complete);
    let app = dreams_router(state);
    let mut ids = Vec::new();
    for session in ["s1", "s2"] {
        let (_, created) = request(
            &app,
            "POST",
            "/v1/dreams",
            Some(create_body("mem", &[session])),
        )
        .await;
        ids.push(created["id"].as_str().unwrap().to_string());
    }
    for id in &ids {
        let _ = wait_for_status(&app, id, "completed").await;
    }
    let (_, first) = request(&app, "GET", "/v1/dreams?limit=1", None).await;
    assert_eq!(first["data"].as_array().unwrap().len(), 1);
    let cursor = first["next_page"].as_str().unwrap();
    let (_, second) = request(
        &app,
        "GET",
        &format!("/v1/dreams?limit=1&page={cursor}"),
        None,
    )
    .await;
    assert_eq!(second["data"].as_array().unwrap().len(), 1);
    let (_, filtered) = request(
        &app,
        "GET",
        "/v1/dreams?statuses=failed&statuses=completed",
        None,
    )
    .await;
    assert_eq!(filtered["data"].as_array().unwrap().len(), 2);
    for uri in [
        "/v1/dreams?limit=0",
        "/v1/dreams?created_at%5Bgt%5D=not-a-time",
    ] {
        assert_eq!(
            request(&app, "GET", uri, None).await.0,
            StatusCode::BAD_REQUEST
        );
    }
}

#[tokio::test]
async fn sqlite_repository_restores_terminal_dreams_after_restart() {
    // Cause/effect graph and decision rules:
    // C1 terminal DreamProcess + linked Session usage U1 -> E1 public lifecycle,
    // output and U1; C2 the same process is reopened while the Session authority
    // reports U2 -> E2 lifecycle/output stay stable and public usage becomes U2;
    // C3 inspect the durable DreamProcess payload -> E3 no duplicated `usage`
    // field. R1=C1, R2=C1+C2, R3=C3 prove process persistence and execution-fact
    // projection have one owner each.
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "awaken-dream-state-{}-{unique}.db",
        std::process::id()
    ));
    let worker = Arc::new(Worker {
        outcome: Outcome::Complete,
        started: Arc::new(Notify::new()),
        release: Arc::new(Notify::new()),
    });
    let repository =
        Arc::new(SqliteManagedSessionRepository::open(path.to_str().unwrap()).unwrap());
    let state = Arc::new(DreamApplication::with_store(worker, repository.clone()).unwrap());
    state.bind_session_source(Arc::new(FixedSessionFacts));
    let app = dreams_router(state.clone());
    let (_, created) = request(&app, "POST", "/v1/dreams", Some(create_body("mem", &["s"]))).await;
    let id = created["id"].as_str().unwrap().to_string();
    let completed = wait_for_status(&app, &id, "completed").await;
    let durable = serde_json::to_value(&repository.dream_processes().unwrap()[0])
        .expect("R3 typed durable process projection");
    assert!(durable.get("usage").is_none(), "R3");
    drop(app);
    drop(state);

    let reopened = DreamApplication::with_store(
        Arc::new(Worker {
            outcome: Outcome::Complete,
            started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
        }),
        Arc::new(SqliteManagedSessionRepository::open(path.to_str().unwrap()).unwrap()),
    )
    .unwrap();
    reopened.bind_session_source(Arc::new(AlternateSessionFacts));
    let restored = reopened.retrieve("default", &id).unwrap();
    let restored = serde_json::to_value(restored).unwrap();
    assert_eq!(restored["status"], "completed");
    assert_eq!(restored["outputs"], completed["outputs"]);
    assert_eq!(completed["usage"]["input_tokens"], 12, "R1");
    assert_eq!(restored["usage"]["input_tokens"], 99, "R2");
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn resume_incomplete_cas_resets_and_executes_a_durable_running_job() {
    struct CrashAtPrepare {
        entered: Arc<Notify>,
        never: Arc<Notify>,
    }
    #[async_trait::async_trait]
    impl DreamExecutor for CrashAtPrepare {
        async fn validate_inputs(&self, _request: &DreamRequest) -> Result<(), DreamFailure> {
            Ok(())
        }
        async fn prepare(&self, _request: &DreamRequest) -> Result<DreamPreparation, DreamFailure> {
            self.entered.notify_one();
            self.never.notified().await;
            unreachable!()
        }
        async fn execute(
            &self,
            _request: &DreamRequest,
            _preparation: &DreamPreparation,
            _cancellation: DreamCancellation,
        ) -> Result<(), DreamFailure> {
            unreachable!()
        }
    }

    // Recovery decision rule: C1 durable status is Running when a process dies
    // before preparation -> E1 a new state CAS-resets it to Pending, E2 exactly
    // the canonical worker path resumes it, E3 terminal output/usage persist.
    let repository = Arc::new(SqliteManagedSessionRepository::open_in_memory().unwrap());
    let entered = Arc::new(Notify::new());
    let first = Arc::new(
        DreamApplication::with_store(
            Arc::new(CrashAtPrepare {
                entered: entered.clone(),
                never: Arc::new(Notify::new()),
            }),
            repository.clone(),
        )
        .unwrap(),
    );
    let first_app = dreams_router(first);
    let (_, created) = request(
        &first_app,
        "POST",
        "/v1/dreams",
        Some(create_body("mem", &["s"])),
    )
    .await;
    entered.notified().await;

    let recovered = Arc::new(
        DreamApplication::with_store(
            Arc::new(Worker {
                outcome: Outcome::Complete,
                started: Arc::new(Notify::new()),
                release: Arc::new(Notify::new()),
            }),
            repository,
        )
        .unwrap(),
    );
    recovered.bind_session_source(Arc::new(FixedSessionFacts));
    recovered.resume_incomplete();
    let recovered_app = dreams_router(recovered);
    let terminal =
        wait_for_status(&recovered_app, created["id"].as_str().unwrap(), "completed").await;
    assert_eq!(terminal["usage"]["input_tokens"], 12);
}

#[tokio::test]
async fn restart_retries_terminal_cleanup_before_publishing_completion() {
    struct CleanupFails;
    #[async_trait::async_trait]
    impl DreamExecutor for CleanupFails {
        async fn validate_inputs(&self, _request: &DreamRequest) -> Result<(), DreamFailure> {
            Ok(())
        }
        async fn prepare(&self, request: &DreamRequest) -> Result<DreamPreparation, DreamFailure> {
            Ok(DreamPreparation {
                result_memory_store_id: format!("result-{}", request.job_id),
                session_id: format!("session-{}", request.job_id),
                transcript_file_ids: vec!["transcript-file".into()],
            })
        }
        async fn execute(
            &self,
            _request: &DreamRequest,
            _preparation: &DreamPreparation,
            _cancellation: DreamCancellation,
        ) -> Result<(), DreamFailure> {
            Ok(())
        }
        async fn cleanup(
            &self,
            _request: &DreamRequest,
            _preparation: Option<&DreamPreparation>,
        ) -> Result<(), DreamFailure> {
            Err(DreamFailure::new("internal_error", "cleanup unavailable"))
        }
    }

    // Cleanup decision rule: C1 execution outcome is durable but Session/File
    // cleanup fails -> E1 public projection stays Running and cleanup_pending is
    // durable; C2 restart with healthy cleanup -> E2 cleanup-only resume clears
    // artifacts and only then publishes Completed (the Agent is not re-executed).
    let repository = Arc::new(SqliteManagedSessionRepository::open_in_memory().unwrap());
    let first =
        Arc::new(DreamApplication::with_store(Arc::new(CleanupFails), repository.clone()).unwrap());
    let first_app = dreams_router(first);
    let (_, created) = request(
        &first_app,
        "POST",
        "/v1/dreams",
        Some(create_body("mem", &["s"])),
    )
    .await;
    for _ in 0..100 {
        if repository
            .dream_processes()
            .unwrap()
            .iter()
            .any(|job| job.cleanup_pending)
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    let (_, pending) = request(
        &first_app,
        "GET",
        &format!("/v1/dreams/{}", created["id"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(pending["status"], "running");

    let recovered = Arc::new(
        DreamApplication::with_store(
            Arc::new(Worker {
                outcome: Outcome::Complete,
                started: Arc::new(Notify::new()),
                release: Arc::new(Notify::new()),
            }),
            repository,
        )
        .unwrap(),
    );
    recovered.resume_incomplete();
    let terminal = wait_for_status(
        &dreams_router(recovered),
        created["id"].as_str().unwrap(),
        "completed",
    )
    .await;
    assert_eq!(terminal["outputs"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn dream_uses_one_stable_ordinary_agent_id_without_selection_state() {
    struct RecordingWorker(Arc<std::sync::Mutex<Vec<String>>>);
    #[async_trait::async_trait]
    impl DreamExecutor for RecordingWorker {
        async fn validate_inputs(&self, request: &DreamRequest) -> Result<(), DreamFailure> {
            self.0.lock().unwrap().push(request.agent_id.clone());
            Ok(())
        }
        async fn prepare(&self, request: &DreamRequest) -> Result<DreamPreparation, DreamFailure> {
            Ok(DreamPreparation {
                result_memory_store_id: format!("result-{}", request.job_id),
                session_id: format!("session-{}", request.job_id),
                transcript_file_ids: Vec::new(),
            })
        }
        async fn execute(
            &self,
            _request: &DreamRequest,
            _preparation: &DreamPreparation,
            _cancellation: DreamCancellation,
        ) -> Result<(), DreamFailure> {
            Ok(())
        }
    }

    // Agent-identity decision table: every manual/scheduled Dream freezes the
    // same ordinary Agent id; publishing that id through the normal Agent path
    // changes its executable snapshot without creating Dream-specific selection
    // state or a second API.
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let state = Arc::new(in_memory_application(Arc::new(RecordingWorker(
        seen.clone(),
    ))));
    let app = dreams_router(state);
    let _ = request(
        &app,
        "POST",
        "/v1/dreams",
        Some(create_body("mem", &["s1"])),
    )
    .await;
    let _ = request(
        &app,
        "POST",
        "/v1/dreams",
        Some(create_body("mem", &["s2"])),
    )
    .await;
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            BUILT_IN_DREAM_AGENT_ID.to_string(),
            BUILT_IN_DREAM_AGENT_ID.to_string()
        ]
    );
}

#[tokio::test]
async fn input_deleted_after_create_fails_before_terminal_publication() {
    struct LifecycleWorker(std::sync::atomic::AtomicUsize);
    #[async_trait::async_trait]
    impl DreamExecutor for LifecycleWorker {
        async fn validate_inputs(&self, _request: &DreamRequest) -> Result<(), DreamFailure> {
            if self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                Ok(())
            } else {
                Err(DreamFailure::new(
                    "input_session_unavailable",
                    "selected Session was deleted while Dream was running",
                ))
            }
        }
        async fn prepare(&self, request: &DreamRequest) -> Result<DreamPreparation, DreamFailure> {
            Ok(DreamPreparation {
                result_memory_store_id: format!("result-{}", request.job_id),
                session_id: format!("session-{}", request.job_id),
                transcript_file_ids: vec!["file-transcript".into()],
            })
        }
        async fn execute(
            &self,
            _request: &DreamRequest,
            _preparation: &DreamPreparation,
            _cancellation: DreamCancellation,
        ) -> Result<(), DreamFailure> {
            Ok(())
        }
    }

    // Input lifecycle decision rule: C1 inputs exist at create but a selected
    // Session becomes unavailable before completion -> E1 typed failed Dream,
    // E2 no completed state is ever published, E3 prepared output remains listed.
    let state = Arc::new(in_memory_application(Arc::new(LifecycleWorker(
        std::sync::atomic::AtomicUsize::new(0),
    ))));
    let app = dreams_router(state);
    let (_, created) = request(&app, "POST", "/v1/dreams", Some(create_body("mem", &["s"]))).await;
    let failed = wait_for_status(&app, created["id"].as_str().unwrap(), "failed").await;
    assert_eq!(failed["error"]["type"], "input_session_unavailable");
    assert_eq!(failed["outputs"].as_array().unwrap().len(), 1);
}
