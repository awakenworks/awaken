//! Explicit request lifetimes for authenticated Kubernetes transports.

/// Build a finite control transport with no middleware replay of requests.
/// Side-effect retries remain the caller's fresh-authority decision. Consumers
/// select their supported k8s-openapi model version in the final application.
pub fn kubernetes_control_client(config: kube::Config) -> Result<kube::Client, kube::Error> {
    crate::install_rustls_crypto_provider();
    kube::Client::try_from(control_config(config)).map(kubernetes_client_with_verified_absence)
}

/// Build an attached-stream transport without an idle response ceiling.
/// Connect/TLS settings are retained and middleware retries remain disabled.
pub fn kubernetes_streaming_client(config: kube::Config) -> Result<kube::Client, kube::Error> {
    crate::install_rustls_crypto_provider();
    kube::Client::try_from(streaming_config(config)).map(kubernetes_client_with_verified_absence)
}

/// Qualify absence evidence on an injected Kubernetes transport.
/// Standard constructors apply this internally. A partial Status, a proxy error,
/// or disagreement with the HTTP status is a transport failure, never absence.
pub fn kubernetes_client_with_verified_absence(client: kube::Client) -> kube::Client {
    let namespace = client.default_namespace().to_owned();
    let valid_until = *client.valid_until();
    let service = tower::service_fn(move |request: http::Request<kube::client::Body>| {
        let client = client.clone();
        async move {
            let response = client.send(request).await?;
            let status = response.status();
            if !status.is_client_error() && !status.is_server_error() {
                return Ok(response);
            }
            use http_body_util::BodyExt;
            let (parts, body) = response.into_parts();
            let bytes = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                http_body_util::Limited::new(body, 1024 * 1024).collect(),
            )
            .await
            .map_err(|_| invalid_absence())?
            .map_err(|_| invalid_absence())?
            .to_bytes();
            let body = serde_json::from_slice::<serde_json::Value>(&bytes).ok();
            let claims_absence = body
                .as_ref()
                .is_some_and(|body| body["code"] == 404 || body["reason"] == "NotFound");
            if status == http::StatusCode::NOT_FOUND || claims_absence {
                let qualified = body.as_ref().is_some_and(|body| {
                    status == http::StatusCode::NOT_FOUND
                        && body["kind"] == "Status"
                        && body["apiVersion"] == "v1"
                        && body["status"] == "Failure"
                        && body["code"] == 404
                        && body["reason"] == "NotFound"
                        && body["message"].is_string()
                });
                if !qualified {
                    return Err(invalid_absence());
                }
            }
            Ok(http::Response::from_parts(
                parts,
                kube::client::Body::from(bytes),
            ))
        }
    });
    kube::Client::new(service, namespace).with_valid_until(valid_until)
}

fn invalid_absence() -> kube::Error {
    kube::Error::Service(std::io::Error::other("invalid Kubernetes absence response: a complete Failure/NotFound Status must agree with HTTP 404").into())
}

fn control_config(mut config: kube::Config) -> kube::Config {
    // Each side-effect retry must return through the runtime's current-authority
    // checks. Kube's middleware cannot replay a write under an expired claim.
    config.default_retry = false;
    config
        .read_timeout
        .get_or_insert(std::time::Duration::from_secs(295));
    config
        .write_timeout
        .get_or_insert(std::time::Duration::from_secs(295));
    config
}

fn streaming_config(mut config: kube::Config) -> kube::Config {
    config.default_retry = false;
    config.read_timeout = None;
    config.write_timeout = None;
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Causal graph: ACP starts a quiet long-running tool -> kube's ordinary
    // 295-second response timer closes attached exec -> protocol observes EOF ->
    // WorkUnit fails while the tool remains alive in the Pod. FMECA S9/O5/D8.
    // Decision table: control request keeps finite read/write ceilings; attached
    // stream has neither ceiling; connect/TLS/cluster coordinates stay identical.
    fn attached_exec_has_no_response_ceiling_without_weakening_control_timeouts() {
        let control = control_config(kube::Config::new(
            "https://127.0.0.1:6443/".parse().unwrap(),
        ));
        assert!(control.read_timeout.is_some());
        assert!(control.write_timeout.is_some());

        let stream = streaming_config(control.clone());
        assert_eq!(stream.cluster_url, control.cluster_url);
        assert_eq!(stream.connect_timeout, control.connect_timeout);
        assert_eq!(stream.default_namespace, control.default_namespace);
        assert_eq!(stream.tls_server_name, control.tls_server_name);
        assert!(stream.read_timeout.is_none());
        assert!(stream.write_timeout.is_none());
        assert!(!control.default_retry);
        assert!(!stream.default_retry);

        assert!(control.read_timeout.is_some());
        assert!(control.write_timeout.is_some());
    }

    #[test]
    fn explicit_control_deadlines_are_preserved_without_internal_retries() {
        let mut config = kube::Config::new("https://127.0.0.1:6443/".parse().unwrap());
        config.read_timeout = Some(std::time::Duration::from_secs(41));
        config.write_timeout = Some(std::time::Duration::from_secs(43));
        let configured = control_config(config.clone());
        assert_eq!(configured.read_timeout, config.read_timeout);
        assert_eq!(configured.write_timeout, config.write_timeout);
        assert_eq!(configured.connect_timeout, config.connect_timeout);
        assert!(!configured.default_retry);
    }

    #[tokio::test]
    async fn only_complete_http_absence_can_become_an_optional_missing_object() {
        #[derive(Clone, Copy, Debug)]
        enum Expected {
            Present,
            Absent,
            Unknown,
        }
        let complete = serde_json::json!({"apiVersion":"v1", "kind":"Status", "status":"Failure",
            "reason":"NotFound", "message":"the original object is gone", "code":404});
        let encoded = |code, body: serde_json::Value, expected| {
            (code, serde_json::to_vec(&body).unwrap(), expected)
        };
        let mut rows = vec![
            encoded(
                200,
                serde_json::json!({"apiVersion":"v1", "kind":"Pod", "metadata":{"name":"owned","uid":"exact-original"}}),
                Expected::Present,
            ),
            encoded(404, complete.clone(), Expected::Absent),
            encoded(403, complete.clone(), Expected::Unknown),
            encoded(500, complete.clone(), Expected::Unknown),
        ];
        // A1 genuine absence; A2 every required Status coordinate missing or
        // null; A3 body/HTTP disagreement; A4 proxy/oversize payload. Only A1
        // can release original participants. The transport validates before
        // kube's optional getter can erase an error into None.
        for field in ["apiVersion", "kind", "status", "reason", "message", "code"] {
            let mut missing = complete.clone();
            missing.as_object_mut().unwrap().remove(field);
            rows.push(encoded(404, missing, Expected::Unknown));
            let mut null = complete.clone();
            null[field] = serde_json::Value::Null;
            rows.push(encoded(404, null, Expected::Unknown));
        }
        for (field, value) in [
            ("code", serde_json::json!(403)),
            ("reason", serde_json::json!("Forbidden")),
            ("status", serde_json::json!("Success")),
            ("kind", serde_json::json!("Pod")),
        ] {
            let mut body = complete.clone();
            body[field] = value;
            rows.push(encoded(404, body, Expected::Unknown));
        }
        rows.push((404, b"proxy route missing".to_vec(), Expected::Unknown));
        rows.push((404, vec![b'x'; 1024 * 1024 + 1], Expected::Unknown));
        let rows = std::sync::Arc::new(rows);
        let served = rows.clone();
        let app = axum::Router::new().route(
            "/api/v1/namespaces/ns/pods/{index}",
            axum::routing::get(
                move |axum::extract::Path(index): axum::extract::Path<usize>| {
                    let served = served.clone();
                    async move {
                        (
                            http::StatusCode::from_u16(served[index].0).unwrap(),
                            served[index].1.clone(),
                        )
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut outcomes = Vec::new();
        for build in [kubernetes_control_client, kubernetes_streaming_client] {
            let client = build(kube::Config::new(
                format!("http://{address}").parse().unwrap(),
            ))
            .unwrap();
            let pods = kube::Api::<k8s_openapi::api::core::v1::Pod>::namespaced(client, "ns");
            for (index, (_, _, expected)) in rows.iter().enumerate() {
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    pods.get_opt(&index.to_string()),
                )
                .await;
                outcomes.push((index, *expected, result));
            }
        }
        server.abort();
        let _ = server.await;
        for (index, expected, result) in outcomes {
            match expected {
                Expected::Present => assert!(
                    matches!(result, Ok(Ok(Some(ref pod))) if pod.metadata.uid.as_deref() == Some("exact-original")),
                    "{index}: {result:?}"
                ),
                Expected::Absent => assert!(matches!(result, Ok(Ok(None))), "{index}: {result:?}"),
                Expected::Unknown => assert!(matches!(result, Ok(Err(_))), "{index}: {result:?}"),
            }
        }
    }

    #[tokio::test]
    async fn unavailable_create_returns_once_to_the_authority_owner() {
        // An API 503 must reach the caller after exactly one POST. A middleware
        // retry would bypass the runtime's required fresh-authority decision.
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let app = axum::Router::new().route("/api/v1/namespaces/ns/pods", axum::routing::post(move || {
            let observed = observed.clone();
            async move {
                observed.fetch_add(1, Ordering::SeqCst);
                (axum::http::StatusCode::SERVICE_UNAVAILABLE, axum::Json(serde_json::json!({
                    "kind":"Status", "apiVersion":"v1", "status":"Failure", "reason":"ServiceUnavailable",
                    "message":"the authority owner must decide the next attempt", "code":503
                })))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut outcomes = Vec::new();
        for build in [kubernetes_control_client, kubernetes_streaming_client] {
            let client = build(kube::Config::new(
                format!("http://{address}").parse().unwrap(),
            ))
            .unwrap();
            let pods = kube::Api::<k8s_openapi::api::core::v1::Pod>::namespaced(client, "ns");
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                pods.create(
                    &kube::api::PostParams::default(),
                    &serde_json::from_value(serde_json::json!({"metadata":{"name":"owned"}}))
                        .unwrap(),
                ),
            )
            .await;
            outcomes.push((result, calls.load(Ordering::SeqCst)));
        }
        server.abort();
        let _ = server.await;
        for (index, (result, calls)) in outcomes.into_iter().enumerate() {
            assert!(
                matches!(result, Ok(Err(kube::Error::Api(ref error))) if error.code == 503),
                "{result:?}"
            );
            assert_eq!(calls, index + 1);
        }
    }
}
