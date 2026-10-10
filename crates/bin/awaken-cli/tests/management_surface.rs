//! The composed product plane serves the canonical Worker readiness projection.

use awaken_cli::build_ephemeral_all_in_one_router as build_all_in_one_router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_product_plane_can_read_worker_fleet_readiness() {
    // The fleet projection is only useful if it is reachable where a product
    // plane actually asks. Its unit tests prove the decision; this proves the
    // composed server answers the route at all, which is the failure this
    // surface had before: the Coordinator served no `/v1/workers/readiness`,
    // so Flow's hosted fleet adapter received a permanent 404 and reported the
    // work store as unavailable.
    let app = build_all_in_one_router().await;

    let (status, body) = call(
        &app,
        "GET",
        "/v1/workers/readiness?capabilities=coding",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["ready"],
        json!(false),
        "this composition registers no Worker"
    );
    assert_eq!(body["total_workers"], json!(0));
    assert_eq!(
        body["missing_capabilities"],
        json!(["coding"]),
        "an empty fleet offers nothing, so the requirement is named"
    );

    // The weaker question is a different route answer, not an error.
    let (status, body) = call(&app, "GET", "/v1/workers/readiness", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["missing_capabilities"], json!([]));
}
