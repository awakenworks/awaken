//! Environments + the self-hosted work queue over HTTP: env CRUD + archive, and
//! the work lifecycle (list / poll / ack / heartbeat / stop / stats) including the
//! single-active-lease (open-tier single-worker) cap.

use awaken_protocol_managed::{environment_authoring_router, environment_work_router};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

fn app() -> Router {
    let (authoring, execution) = awaken_protocol_managed::test_support::environment_components();
    environment_authoring_router(authoring).merge(environment_work_router(execution))
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    // The official SDK attaches this identity to every Work lease operation;
    // carrying it on all test requests keeps the generic helper wire-faithful
    // without giving each CRUD call a second implementation.
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("anthropic-worker-id", "sdk-test-worker");
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

async fn make_env(app: &Router) -> String {
    let (s, e) = call(
        app,
        "POST",
        "/v1/environments",
        Some(json!({ "name": "prod" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    e["id"].as_str().unwrap().to_string()
}

async fn call_with_worker(
    app: &Router,
    method: &str,
    uri: &str,
    worker_id: &str,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("anthropic-worker-id", worker_id)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

async fn call_with_lease_headers(
    app: &Router,
    method: &str,
    uri: &str,
    worker_id: Option<&str>,
    bearer: Option<&str>,
    api_key: Option<&str>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(worker_id) = worker_id {
        request = request.header("anthropic-worker-id", worker_id);
    }
    if let Some(bearer) = bearer {
        request = request.header("authorization", format!("Bearer {bearer}"));
    }
    if let Some(api_key) = api_key {
        request = request.header("x-api-key", api_key);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

/// Scope cause graph: omitted -> absent; organization/account -> exact echo;
/// update changes scope and revision; invalid enum -> 400 before persistence.
#[tokio::test]
async fn environment_scope_follows_the_official_decision_table() {
    let app = app();

    // A plain create: exactly the official field set, no `scope`.
    let (s, env) = call(
        &app,
        "POST",
        "/v1/environments",
        Some(json!({ "name": "prod" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(env.get("scope").is_none(), "no scope on the wire: {env}");
    let expected: std::collections::BTreeSet<&str> = [
        "id",
        "type",
        "archived_at",
        "created_at",
        "updated_at",
        "name",
        "description",
        "metadata",
        "config",
    ]
    .into_iter()
    .collect();
    let got: std::collections::BTreeSet<&str> = env
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(got, expected, "only the official BetaEnvironment fields");

    // Official organization scope is persisted and echoed.
    let (s, scoped) = call(
        &app,
        "POST",
        "/v1/environments",
        Some(json!({ "name": "prod2", "scope": "organization" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        scoped["scope"] == "organization",
        "official scope is echoed"
    );
    let id = scoped["id"].as_str().unwrap();
    let (s, updated) = call(
        &app,
        "POST",
        &format!("/v1/environments/{id}"),
        Some(json!({"scope":"account"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(updated["scope"], "account");
    let (s, _) = call(
        &app,
        "POST",
        "/v1/environments",
        Some(json!({"name":"bad", "scope":"workspace"})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

/// Environment config admission cause graph:
/// C1 official tagged variant; C2 every nested field belongs to that variant.
/// E1 persist canonical config; E2 reject before creating an Environment/work item.
///
/// | Rule | C1 | C2 | Result |
/// |---|---|---|---|
/// | A1 self_hosted | T | T | canonical self_hosted |
/// | A2 cloud | T | T | defaulted network/packages |
/// | A3 private sandbox/policy/requests/limits extension | T | F | 400 |
/// | A4 unknown variant | F | - | 400 |
/// | A5 unknown nested network/package field | T | F | 400 |
#[tokio::test]
async fn environment_config_admission_follows_the_official_union_decision_table() {
    let app = app();
    let cases = [
        (
            "A3",
            json!({"type":"self_hosted", "sandbox": {"isolation":"container"}}),
        ),
        (
            "A3-requests",
            json!({"type":"cloud", "requests":{"cpu_millis":500}}),
        ),
        (
            "A3-limits",
            json!({"type":"cloud", "limits":{"memory_bytes":1073741824u64}}),
        ),
        (
            "A3-policy",
            json!({"type":"cloud", "sandbox_policy":{"id":"design", "version":1}}),
        ),
        ("A4", json!({"type":"custom_cloud"})),
        (
            "A5-network",
            json!({"type":"cloud", "networking":{"type":"limited", "proxy":"x"}}),
        ),
        (
            "A5-package",
            json!({"type":"cloud", "packages":{"docker":["x"]}}),
        ),
        (
            "A5-host-scheme",
            json!({"type":"cloud", "networking":{"type":"limited", "allowed_hosts":["https://api.test"]}}),
        ),
        (
            "A5-host-port",
            json!({"type":"cloud", "networking":{"type":"limited", "allowed_hosts":["api.test:443"]}}),
        ),
        (
            "A5-host-wildcard",
            json!({"type":"cloud", "networking":{"type":"limited", "allowed_hosts":["*api.test"]}}),
        ),
        (
            "A5-empty-package",
            json!({"type":"cloud", "packages":{"pip":[""]}}),
        ),
        (
            "A5-package-option",
            json!({"type":"cloud", "packages":{"npm":["--registry"]}}),
        ),
    ];
    for (rule, config) in cases {
        let (status, _) = call(
            &app,
            "POST",
            "/v1/environments",
            Some(json!({"name": rule, "config": config})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{rule}");
    }

    let (status, page) = call(&app, "GET", "/v1/environments", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["data"].as_array().unwrap().len(), 1);
    assert_eq!(page["data"][0]["id"], "env_local");
    assert_eq!(
        page["data"][0]["name"], "Local",
        "A3-A5 persist no authored Environment; the immutable built-in remains"
    );

    let (status, cloud) = call(
        &app,
        "POST",
        "/v1/environments",
        Some(json!({"name":"A2", "config":{"type":"cloud"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cloud["config"]["networking"]["type"], "unrestricted");
    for manager in ["apt", "cargo", "gem", "go", "npm", "pip"] {
        assert_eq!(cloud["config"]["packages"][manager], json!([]), "{manager}");
    }
}

/// Update cause graph: a present Cloud patch changes only present nested fields;
/// omitted fields preserve aggregate state while explicit null resets the field.
/// The store applies this mutation atomically with the Environment revision.
///
/// | Rule | Field | Input | Effect |
/// |---|---|---|---|
/// | U1 | limited hosts/package flag | omitted | preserved |
/// | U2 | MCP flag | false | replaced |
/// | U3 | npm | null | cleared; pip preserved |
/// | U4 | networking | null | unrestricted; packages preserved |
/// | U5 | private requests/limits/policy | present | 400; revision/config unchanged |
#[tokio::test]
async fn environment_update_preserves_omitted_and_resets_null_fields() {
    let app = app();
    let (status, created) = call(
        &app,
        "POST",
        "/v1/environments",
        Some(json!({
            "name": "patchable",
            "config": {
                "type": "cloud",
                "networking": {
                    "type": "limited",
                    "allowed_hosts": ["api.example.test"],
                    "allow_mcp_servers": true,
                    "allow_package_managers": true
                },
                "packages": { "type": "packages", "npm": ["tsx"], "pip": ["httpx"] }
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = created["id"].as_str().unwrap();

    let (status, patched) = call(
        &app,
        "POST",
        &format!("/v1/environments/{id}"),
        Some(json!({
            "config": {
                "type": "cloud",
                "networking": { "type": "limited", "allow_mcp_servers": false },
                "packages": { "type": "packages", "npm": null }
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        patched["config"]["networking"]["allowed_hosts"],
        json!(["api.example.test"]),
        "U1"
    );
    assert_eq!(
        patched["config"]["networking"]["allow_mcp_servers"], false,
        "U2"
    );
    assert_eq!(
        patched["config"]["networking"]["allow_package_managers"], true,
        "U1"
    );
    assert_eq!(patched["config"]["packages"]["npm"], json!([]), "U3");
    assert_eq!(patched["config"]["packages"]["pip"], json!(["httpx"]), "U3");

    let (status, reset) = call(
        &app,
        "POST",
        &format!("/v1/environments/{id}"),
        Some(json!({ "config": { "type": "cloud", "networking": null } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(reset["config"]["networking"]["type"], "unrestricted", "U4");
    assert_eq!(reset["config"]["packages"]["pip"], json!(["httpx"]), "U4");

    for private in [
        json!({"requests":{"cpu_millis":500}}),
        json!({"limits":{"memory_bytes":1073741824u64}}),
        json!({"sandbox_policy":{"id":"design", "version":1}}),
    ] {
        let mut config = json!({"type":"cloud"});
        config.as_object_mut().unwrap().extend(
            private
                .as_object()
                .unwrap()
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
        let (status, _) = call(
            &app,
            "POST",
            &format!("/v1/environments/{id}"),
            Some(json!({"config": config})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "U5");
    }
    let (status, unchanged) = call(&app, "GET", &format!("/v1/environments/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "U5");
    assert_eq!(unchanged["config"], reset["config"], "U5");
}

#[tokio::test]
async fn official_worker_header_and_heartbeat_cas_are_wired() {
    // Worker-mutation cause/effect graph: C1 caller owns the live lease; C2
    // heartbeat compare token matches; C3 caller is another Worker. Effects:
    // E1 apply ack/heartbeat/stop, E2 reject atomically with HTTP 412. The
    // owner condition dominates the heartbeat token, so a shared first-token
    // value cannot transfer authority.
    //
    // | Rule | Owner | Token | Mutation | Effect |
    // |---|---|---|---|---|
    // | O0 | absent | n/a | poll | 400 before claim |
    // | O1 | exact | first/current | heartbeat | E1 |
    // | O2 | other | any | heartbeat | E2 |
    // | O3 | other | n/a | ack/stop | E2 |
    // | O4 | exact | n/a | ack/stop | E1 |
    let app = app();
    let id = make_env(&app).await;
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/v1/environments/{id}/work/poll?block_ms="))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST, "O0");
    let (status, work) = call_with_worker(
        &app,
        "GET",
        &format!("/v1/environments/{id}/work/poll"),
        "worker-cas",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let wid = work["id"].as_str().unwrap();

    let (_, stats) = call(
        &app,
        "GET",
        &format!("/v1/environments/{id}/work/stats"),
        None,
    )
    .await;
    assert_eq!(stats["workers_polling"], 1);

    // Before any heartbeat receipt exists, the claim owner is still authority:
    // another worker cannot win the otherwise-shared NO_HEARTBEAT condition.
    let (status, _) = call_with_worker(
        &app,
        "POST",
        &format!("/v1/environments/{id}/work/{wid}/heartbeat?expected_last_heartbeat=NO_HEARTBEAT"),
        "worker-other",
    )
    .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);

    let (status, first) = call_with_worker(
        &app,
        "POST",
        &format!(
            "/v1/environments/{id}/work/{wid}/heartbeat?expected_last_heartbeat=NO_HEARTBEAT&desired_ttl_seconds=7"
        ),
        "worker-cas",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["ttl_seconds"], 7);
    let first_token = first["last_heartbeat"].as_str().unwrap();

    let (status, _) = call_with_worker(
        &app,
        "POST",
        &format!("/v1/environments/{id}/work/{wid}/heartbeat?expected_last_heartbeat=NO_HEARTBEAT"),
        "worker-cas",
    )
    .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);

    let (status, second) = call_with_worker(
        &app,
        "POST",
        &format!(
            "/v1/environments/{id}/work/{wid}/heartbeat?expected_last_heartbeat={first_token}"
        ),
        "worker-cas",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(second["last_heartbeat"], first["last_heartbeat"]);

    let (status, _) = call_with_worker(
        &app,
        "POST",
        &format!(
            "/v1/environments/{id}/work/{wid}/heartbeat?expected_last_heartbeat={first_token}"
        ),
        "worker-cas",
    )
    .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);

    for operation in ["ack", "stop"] {
        let (status, _) = call_with_worker(
            &app,
            "POST",
            &format!("/v1/environments/{id}/work/{wid}/{operation}"),
            "worker-other",
        )
        .await;
        assert_eq!(status, StatusCode::PRECONDITION_FAILED, "O3 {operation}");
    }
    let (status, acknowledged) = call_with_worker(
        &app,
        "POST",
        &format!("/v1/environments/{id}/work/{wid}/ack"),
        "worker-cas",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "O4 ack");
    assert!(acknowledged["acknowledged_at"].is_string(), "O4 ack");
    let (status, stopped) = call_with_worker(
        &app,
        "POST",
        &format!("/v1/environments/{id}/work/{wid}/stop"),
        "worker-cas",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "O4 stop");
    assert_eq!(stopped["state"], "stopped", "O4 stop");
    let (status, _) = call_with_worker(
        &app,
        "POST",
        &format!("/v1/environments/{id}/work/{wid}/stop"),
        "worker-cas",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "the official EnvironmentWorker treats 409 as already stopped"
    );
}

#[tokio::test]
async fn official_sdk_credential_owns_optional_worker_id_lease_lifecycle() {
    // Official-helper cause/effect graph: C1 poll carries Environment bearer K
    // plus observational Worker ID W; C2 ack/heartbeat/stop carry K but omit W,
    // exactly as @anthropic-ai/sdk WorkPoller does; C3 a mutation carries other
    // bearer J; C4 neither credential nor W is present; C5 raw SDK calls carry
    // API key K without W. Effects: E1 the queue records
    // W as polling but atomically leases to an opaque K fingerprint; E2 C1+C2
    // continues one lease; E3 C3 is 412; E4 C4 is 400 before mutation.
    // FMECA: requiring W on every mutation breaks the official helper, while
    // treating missing W as unowned bypasses fencing. Separating observation
    // from the credential-derived lease owner preserves both without a second
    // registry or raw-secret persistence.
    //
    // | Rule | poll K+W | mutation bearer | Worker header | Effect |
    // |---|---|---|---|---|
    // | P1 | yes | K | omitted | E1+E2 |
    // | P2 | yes | J | omitted | E3 |
    // | P3 | yes | omitted | omitted | E4 |
    // | P4 | API-key K, no W | K | omitted | E1+E2 |
    let app = app();
    let id = make_env(&app).await;
    let (status, work) = call_with_lease_headers(
        &app,
        "GET",
        &format!("/v1/environments/{id}/work/poll?block_ms="),
        Some("official-poller-7"),
        Some("environment-key-k"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "P1");
    let wid = work["id"].as_str().unwrap();

    let (_, stats) = call(
        &app,
        "GET",
        &format!("/v1/environments/{id}/work/stats"),
        None,
    )
    .await;
    assert_eq!(stats["workers_polling"], 1, "P1 observes W, not K and W");

    let (status, acknowledged) = call_with_lease_headers(
        &app,
        "POST",
        &format!("/v1/environments/{id}/work/{wid}/ack"),
        None,
        Some("environment-key-k"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "P1");
    assert!(acknowledged["acknowledged_at"].is_string(), "P1");

    let (status, _) = call_with_lease_headers(
        &app,
        "POST",
        &format!("/v1/environments/{id}/work/{wid}/heartbeat"),
        None,
        Some("environment-key-j"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED, "P2");

    let (status, _) = call_with_lease_headers(
        &app,
        "POST",
        &format!("/v1/environments/{id}/work/{wid}/stop"),
        None,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "P3");

    let (status, stopped) = call_with_lease_headers(
        &app,
        "POST",
        &format!("/v1/environments/{id}/work/{wid}/stop"),
        None,
        Some("environment-key-k"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "P1");
    assert_eq!(stopped["state"], "stopped", "P1");

    // The generated raw SDK method makes Worker ID optional and authenticates
    // with X-Api-Key. That credential owns and observes the lease when no label
    // is present; it is not converted into an anonymous/unfenced mutation.
    let raw_id = make_env(&app).await;
    let (status, raw_work) = call_with_lease_headers(
        &app,
        "GET",
        &format!("/v1/environments/{raw_id}/work/poll?block_ms="),
        None,
        None,
        Some("raw-environment-key"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "P4");
    let raw_wid = raw_work["id"].as_str().unwrap();
    let (status, raw_ack) = call_with_lease_headers(
        &app,
        "POST",
        &format!("/v1/environments/{raw_id}/work/{raw_wid}/ack"),
        None,
        None,
        Some("raw-environment-key"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "P4");
    assert!(raw_ack["acknowledged_at"].is_string(), "P4");
}

#[tokio::test]
async fn work_poll_honors_documented_blocking_partitions_and_boundaries() {
    // Causes: empty queue and block_ms omitted, explicit null, 1..=999, or out of range.
    // Constraints: null is the non-blocking partition; numeric values must be inclusive 1..=999.
    // Effects: immediate null, bounded long-poll null, documented default wait, or atomic 400.
    // Decision rule: self-hosted-sandboxes poll P1-P5.
    let app = app();
    let id = make_env(&app).await;

    // Drain the seeded healthcheck using the SDK's `null` spelling (`block_ms=`),
    // then stop it so every following poll observes an empty claimable queue.
    let (status, leased) = call(
        &app,
        "GET",
        &format!("/v1/environments/{id}/work/poll?block_ms="),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let work_id = leased["id"].as_str().unwrap();
    let (status, _) = call(
        &app,
        "POST",
        &format!("/v1/environments/{id}/work/{work_id}/stop?force=true"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let started = tokio::time::Instant::now();
    let (status, empty) = call(
        &app,
        "GET",
        &format!("/v1/environments/{id}/work/poll?block_ms="),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(empty.is_null());
    assert!(
        started.elapsed() < std::time::Duration::from_millis(100),
        "P1"
    );

    let started = tokio::time::Instant::now();
    let (status, empty) = call(
        &app,
        "GET",
        &format!("/v1/environments/{id}/work/poll?block_ms=40"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(empty.is_null());
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(35),
        "P2"
    );

    let started = tokio::time::Instant::now();
    let (status, empty) = call(
        &app,
        "GET",
        &format!("/v1/environments/{id}/work/poll"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(empty.is_null());
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(900),
        "P3"
    );

    for invalid in ["0", "1000", "not-a-number"] {
        let (status, _) = call(
            &app,
            "GET",
            &format!("/v1/environments/{id}/work/poll?block_ms={invalid}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "P4/P5: {invalid}");
    }
}

/// The sole snapshot compiler normalizes all networking wire shapes. In
/// particular, an empty limited allowlist is exactly `None`, not a parallel
/// spelling that would demand an unsupported allowlist provider.
#[tokio::test]
async fn snapshot_normalizes_the_networking_policy() {
    let (authoring, execution) = awaken_protocol_managed::test_support::environment_components();
    let app =
        environment_authoring_router(authoring).merge(environment_work_router(execution.clone()));
    async fn make(app: &Router, cfg: Value) -> String {
        let (s, e) = call(
            app,
            "POST",
            "/v1/environments",
            Some(json!({ "name": "e", "config": cfg })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        e["id"].as_str().unwrap().to_string()
    }

    let limited = make(
        &app,
        json!({ "type": "cloud", "networking": { "type": "limited" } }),
    )
    .await;
    assert_eq!(
        execution
            .snapshot(&limited, None)
            .await
            .unwrap()
            .unwrap()
            .network,
        awaken_session_contract::SessionNetworkPolicy::None,
        "empty limited allowlist canonicalizes to no network"
    );

    let unrestricted = make(
        &app,
        json!({ "type": "cloud", "networking": { "type": "unrestricted" } }),
    )
    .await;
    assert_eq!(
        execution
            .snapshot(&unrestricted, None)
            .await
            .unwrap()
            .unwrap()
            .network,
        awaken_session_contract::SessionNetworkPolicy::Unrestricted,
        "unrestricted keeps host network"
    );

    let self_hosted = make(&app, json!({ "type": "self_hosted" })).await;
    assert_eq!(
        execution
            .snapshot(&self_hosted, None)
            .await
            .unwrap()
            .unwrap()
            .network,
        awaken_session_contract::SessionNetworkPolicy::Unrestricted,
        "absent networking keeps host network"
    );

    assert!(
        execution
            .snapshot("env_nonexistent", None)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn environment_application_replay_converges_one_environment_and_healthcheck() {
    // Causal graph: C1 one startup -> E1 every protocol accessor returns the
    // same application allocation; C2 first stable command -> E2 Environment plus
    // healthcheck; C3 exact replay -> E3 same Environment and one healthcheck; C4
    // conflicting payload -> E4 registry rejection before another side effect.
    // Decision table: R1=C1=>E1; R2=C2=>E2; R3=C2+C3=>E3; R4=C2+C4=>E4.
    let (authoring, execution) = awaken_protocol_managed::test_support::environment_components();
    assert!(
        std::sync::Arc::ptr_eq(&authoring.application(), &authoring.application()),
        "R1: HTTP and assistant adapters must share one application instance"
    );
    let command = awaken_environment_contract::CreateEnvironmentCommand {
        command_id: "control:call-1".into(),
        name: "stable".into(),
        description: String::new(),
        metadata: Default::default(),
        scope: None,
        config: awaken_environment_contract::EnvironmentConfig::SelfHosted,
    };
    let first = authoring
        .application()
        .create(command.clone())
        .await
        .unwrap();
    let replay = authoring.application().create(command).await.unwrap();
    assert_eq!(first.id, replay.id, "R1/R2");
    let app = environment_authoring_router(authoring).merge(environment_work_router(execution));
    let (_, work) = call(
        &app,
        "GET",
        &format!("/v1/environments/{}/work", first.id),
        None,
    )
    .await;
    assert_eq!(work["data"].as_array().unwrap().len(), 1, "R2");
}

#[tokio::test]
async fn executable_registration_seeds_work_only_for_self_hosted_environments() {
    // Cause/effect decision table: R1 SelfHosted registration owns an external
    // worker queue and seeds exactly one healthcheck; R2 Cloud registration is
    // executed by Awaken and therefore creates no external worker work item.
    let app = app();
    let self_hosted = make_env(&app).await;
    let (_, self_hosted_work) = call(
        &app,
        "GET",
        &format!("/v1/environments/{self_hosted}/work"),
        None,
    )
    .await;
    assert_eq!(self_hosted_work["data"].as_array().unwrap().len(), 1, "R1");

    let (status, cloud) = call(
        &app,
        "POST",
        "/v1/environments",
        Some(json!({
            "name": "cloud",
            "config": {"type": "cloud", "packages": {"type": "packages"}}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let cloud_id = cloud["id"].as_str().unwrap();
    let (_, cloud_work) = call(
        &app,
        "GET",
        &format!("/v1/environments/{cloud_id}/work"),
        None,
    )
    .await;
    assert!(cloud_work["data"].as_array().unwrap().is_empty(), "R2");
}

#[tokio::test]
async fn environment_crud_and_work_lifecycle() {
    let app = app();

    // Create — config defaults to self_hosted; a healthcheck work item is seeded.
    let (s, env) = call(
        &app,
        "POST",
        "/v1/environments",
        Some(json!({ "name": "prod" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(env["type"], "environment");
    assert_eq!(env["config"]["type"], "self_hosted");
    let id = env["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("env_"));

    // Work list shows the seeded queued item.
    let (_, list) = call(&app, "GET", &format!("/v1/environments/{id}/work"), None).await;
    let works = list["data"].as_array().unwrap();
    assert_eq!(works.len(), 1);
    assert_eq!(works[0]["state"], "queued");
    assert_eq!(works[0]["data"]["type"], "healthcheck");

    // Stats: one queued, depth 1.
    let (_, stats) = call(
        &app,
        "GET",
        &format!("/v1/environments/{id}/work/stats"),
        None,
    )
    .await;
    assert_eq!(stats["type"], "work_queue_stats");
    assert_eq!(stats["depth"], 1);
    // Nothing claimed yet: the seeded healthcheck is queued, so `pending` (claimed &
    // processing) is 0, not the queue depth.
    assert_eq!(stats["pending"], 0);

    // Poll leases the item -> active.
    let (s, leased) = call(
        &app,
        "GET",
        &format!("/v1/environments/{id}/work/poll"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(leased["state"], "active");
    assert!(leased["started_at"].is_string());
    let wid = leased["id"].as_str().unwrap().to_string();

    // A second poll returns null (single active lease == the single-worker cap).
    let (_, again) = call(
        &app,
        "GET",
        &format!("/v1/environments/{id}/work/poll"),
        None,
    )
    .await;
    assert!(again.is_null(), "only one active lease at a time");

    // Ack + heartbeat + stop.
    let (_, acked) = call(
        &app,
        "POST",
        &format!("/v1/environments/{id}/work/{wid}/ack"),
        None,
    )
    .await;
    assert!(acked["acknowledged_at"].is_string());
    let (_, hb) = call(
        &app,
        "POST",
        &format!("/v1/environments/{id}/work/{wid}/heartbeat"),
        None,
    )
    .await;
    assert_eq!(hb["type"], "work_heartbeat");
    assert_eq!(hb["lease_extended"], true);
    assert_eq!(hb["ttl_seconds"], 60);
    let (_, stopped) = call(
        &app,
        "POST",
        &format!("/v1/environments/{id}/work/{wid}/stop"),
        None,
    )
    .await;
    assert_eq!(stopped["state"], "stopped");
    assert!(stopped["stop_requested_at"].is_string());

    // Retrieve + update work (metadata).
    let (_, upd) = call(
        &app,
        "POST",
        &format!("/v1/environments/{id}/work/{wid}"),
        Some(json!({ "metadata": { "run": "1", "remove_me": "yes" } })),
    )
    .await;
    assert_eq!(upd["metadata"]["run"], "1");
    let (_, deleted_metadata) = call(
        &app,
        "POST",
        &format!("/v1/environments/{id}/work/{wid}"),
        Some(json!({ "metadata": { "run": "2", "remove_me": null } })),
    )
    .await;
    assert_eq!(deleted_metadata["metadata"]["run"], "2");
    assert!(
        deleted_metadata["metadata"].get("remove_me").is_none(),
        "a null metadata patch deletes the key"
    );

    // Env retrieve / update / list / archive.
    let (_, upenv) = call(
        &app,
        "POST",
        &format!("/v1/environments/{id}"),
        Some(json!({ "description": "the prod env" })),
    )
    .await;
    assert_eq!(upenv["description"], "the prod env");
    let (_, page) = call(&app, "GET", "/v1/environments", None).await;
    assert_eq!(page["data"].as_array().unwrap()[0]["id"], id);
    let (_, arch) = call(
        &app,
        "POST",
        &format!("/v1/environments/{id}/archive"),
        None,
    )
    .await;
    assert!(arch["archived_at"].is_string());
    let (s, del) = call(&app, "DELETE", &format!("/v1/environments/{id}"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(del["type"], "environment_deleted");
}

#[tokio::test]
async fn work_and_env_not_found_paths() {
    let app = app();
    let id = make_env(&app).await;
    // Work under the wrong env 404s.
    let (s, _) = call(&app, "GET", "/v1/environments/env_missing/work/poll", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(
        &app,
        "GET",
        &format!("/v1/environments/{id}/work/work_missing"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(&app, "GET", "/v1/environments/env_missing", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}
