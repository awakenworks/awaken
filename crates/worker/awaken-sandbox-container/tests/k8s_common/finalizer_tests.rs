use super::k8s_common::set_test_finalizer;
use std::sync::{Arc, Mutex};

#[tokio::test(start_paused = true)]
async fn test_barrier_rebases_only_same_uid_conflicts_and_preserves_other_finalizers() {
    // Causes: C1 hold/release and already desired state; C2 concurrent metadata
    // writer; C3 original UID/version remain present; C4 API succeeds, conflicts,
    // denies or stalls. Effects: E1 add/remove only our barrier; E2 fresh version
    // preserves a concurrently added production finalizer; E3 no mutation of a
    // replacement; E4 non-conflicts fail immediately; E5 retry/time budgets end.
    // Decision rows below cross add/remove/idempotence, two recoverable conflicts,
    // UID replacement, missing coordinates, permanent denial and bounded failure.
    // Negative rows require the exact intended panic boundary as well as effect
    // counts; a broken mock assertion or unrelated panic cannot qualify them.
    // This is the single test owner for the shared Pod/PVC fixture. The real PVC
    // KPV4 test proves the same helper against concurrent Kubernetes controllers.
    for (mode, hold, initially_held, succeeds, patches) in [
        ("ok", true, false, true, 1),
        ("ok", false, true, true, 1),
        ("ok", true, true, true, 0),
        ("ok", false, false, true, 0),
        ("conflicts", true, false, true, 3),
        ("conflicts", false, true, true, 3),
        ("replacement", true, false, false, 1),
        ("forbidden", true, false, false, 1),
        ("no-uid", true, false, false, 0),
        ("no-version", true, false, false, 0),
        ("always-conflict", true, false, false, 8),
        ("hang", true, false, false, 0),
    ] {
        let mut finalizers = vec!["production/base".to_string()];
        if initially_held {
            finalizers.push("test/barrier".into());
        }
        let state = Arc::new(Mutex::new((
            0usize,
            Vec::<serde_json::Value>::new(),
            finalizers,
        )));
        let observed = state.clone();
        let service = tower::service_fn(move |request: http::Request<kube::client::Body>| {
            let observed = observed.clone();
            async move {
                if mode == "hang" {
                    return std::future::pending::<
                        Result<http::Response<kube::client::Body>, std::convert::Infallible>,
                    >()
                    .await;
                }
                let method = request.method().clone();
                let body = request.into_body().collect_bytes().await.unwrap();
                let mut state = observed.lock().unwrap();
                let mut code = 200;
                if method == http::Method::GET {
                    state.0 += 1;
                } else {
                    assert_eq!(method, http::Method::PATCH);
                    let patch: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(patch["metadata"]["uid"], "original", "E3");
                    assert_eq!(
                        patch["metadata"]["resourceVersion"],
                        state.0.to_string(),
                        "E2"
                    );
                    state.1.push(patch.clone());
                    if mode == "always-conflict"
                        || mode == "replacement"
                        || (mode == "conflicts" && state.1.len() <= 2)
                    {
                        code = 409;
                        if !state.2.iter().any(|value| value == "production/late") {
                            state.2.push("production/late".into());
                        }
                    } else if mode == "forbidden" {
                        code = 403;
                    } else {
                        state.2 = serde_json::from_value(patch["metadata"]["finalizers"].clone())
                            .unwrap();
                    }
                }
                let value = if code == 200 {
                    serde_json::json!({"apiVersion":"v1", "kind":"Pod", "metadata": {
                        "name":"barrier", "namespace":"default",
                        "uid": if mode == "no-uid" { None } else if mode == "replacement" && state.0 > 1 { Some("replacement") } else { Some("original") },
                        "resourceVersion": if mode == "no-version" { None } else { Some(state.0.to_string()) },
                        "finalizers":state.2
                    }})
                } else {
                    serde_json::json!({"apiVersion":"v1", "kind":"Status", "status":"Failure",
                        "reason":"fixture rejection", "message":"fixture rejection", "code":code})
                };
                Ok::<_, std::convert::Infallible>(
                    http::Response::builder()
                        .status(code)
                        .header("content-type", "application/json")
                        .body(kube::client::Body::from(
                            serde_json::to_vec(&value).unwrap(),
                        ))
                        .unwrap(),
                )
            }
        });
        let client = kube::Client::new(service, "default");
        let api = kube::Api::<k8s_openapi::api::core::v1::Pod>::namespaced(client, "default");
        let result = tokio::spawn(async move {
            set_test_finalizer(&api, "barrier", "test/barrier", hold).await;
        })
        .await;
        assert_eq!(result.is_ok(), succeeds, "{mode}/{hold}: E4/E5");
        if !succeeds {
            let panic = result.unwrap_err().into_panic();
            let message = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .expect("fixture failures carry a diagnostic");
            let expected = match mode {
                "replacement" => "test barrier object was replaced",
                "forbidden" | "always-conflict" => "patch test-owned deletion barrier:",
                "no-uid" => "test barrier object UID",
                "no-version" => "test barrier object version",
                "hang" => "test barrier metadata deadline",
                _ => unreachable!("all negative rows name their failure boundary"),
            };
            assert!(message.contains(expected), "{mode}: {message}");
        }
        let state = state.lock().unwrap();
        assert_eq!(state.1.len(), patches, "{mode}/{hold}: exact effects");
        assert!(state.2.iter().any(|value| value == "production/base"), "E1");
        if succeeds {
            assert_eq!(
                state.2.iter().any(|value| value == "test/barrier"),
                hold,
                "E1"
            );
            if mode == "conflicts" {
                assert!(state.2.iter().any(|value| value == "production/late"), "E2");
            }
        }
    }
}
