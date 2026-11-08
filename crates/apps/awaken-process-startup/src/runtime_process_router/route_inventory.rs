// @formal surface exclude=test_only
// @formal owner crates/apps/awaken-process-startup/src/runtime_process_router.rs
// @formal reason: Route-inventory oracle included only under cfg(test); it probes the assembled
// @formal reason: process with TRACE and owns no production authorization decision.
//! Route inventory of the assembled runtime process.
//!
//! The runtime process is one composition root that mounts every protocol and
//! management router, then wraps them in IAM, audit, rate-limit, Work-session,
//! workspace-path and reconciliation layers. Splitting that assembly must not
//! change which path is served by which surface, nor which guard answers first.
//!
//! Cause/effect: for each process shape (AllInOne without IAM, AllInOne with the
//! embedded IAM guard, split Coordinator) and each of its two listeners (public,
//! private), every candidate path is probed with `TRACE`, a method no route
//! handles. The answer observes the assembly without running a handler:
//! * unmounted path: the router's fallback status (404);
//! * mounted path: 405 with `allow` naming the exact method set;
//! * a guard or layer in front of the route: its own status and headers.
//!
//! Each path is probed flat and, for `/v1/` paths, through the workspace
//! address `/v1/workspaces/{ws}/…`. The pinned table lives in
//! `route_inventory.txt`; its first column is the candidate set. To add a
//! route, append its path and rerun with `AWAKEN_WRITE_ROUTE_INVENTORY=1`.

use crate::config::Role;
use crate::hosted_model_publication::PublicationModelSupply;
use crate::local_process_stores::{in_memory_process_stores, in_memory_split_coordinator};
use crate::prepared_process::ProcessRouters;
use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request};
use std::time::Duration;
use tower::ServiceExt;

const INVENTORY: &str = include_str!("route_inventory.txt");
const INVENTORY_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/runtime_process_router/route_inventory.txt"
);
const HEADER: &str = "\
# Route inventory of the runtime process; see route_inventory.rs.
# Columns: path | all-in-one public | all-in-one private | secured all-in-one public
#          | coordinator public | coordinator private
# Cell: TRACE status, `allow=` methods on 405, `h=` response header names.
";

/// One process shape whose listeners are probed.
enum Shape {
    AllInOne,
    SecuredAllInOne,
    Coordinator,
}

async fn assemble(shape: Shape, iam_root: &std::path::Path) -> ProcessRouters {
    let (stores, control_service, role) = match shape {
        Shape::Coordinator => {
            let (stores, services) = in_memory_split_coordinator().await;
            (stores, Some(services), Role::Coordinator)
        }
        Shape::AllInOne | Shape::SecuredAllInOne => {
            (in_memory_process_stores().await, None, Role::AllInOne)
        }
    };
    let iam = matches!(shape, Shape::SecuredAllInOne).then(|| {
        awaken_control::embedded_iam(iam_root, stores.clock.clone())
            .expect("prepare secured route inventory IAM")
    });
    let mut process = crate::exact_host_model::registered_test_process_options(&stores);
    // Physical execution placement installs Host executors, never routes:
    // every shape uses the registered Worker placement the product ships.
    process.role = role;
    process.control_service = control_service;
    super::prepare_runtime_routers(
        stores,
        iam,
        None,
        None,
        PublicationModelSupply::PublishedProviders,
        process,
    )
    .await
    .expect("assemble runtime process for its route inventory")
}

fn concrete(path: &str) -> String {
    path.split('/')
        .map(|segment| {
            if segment.starts_with('{') && segment.ends_with('}') {
                "probe"
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

async fn probe(router: &Router, path: &str) -> String {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::TRACE)
                .uri(path)
                .body(Body::empty())
                .expect("probe request"),
        )
        .await
        .expect("router is infallible");
    let mut cell = response.status().as_u16().to_string();
    if let Some(allow) = response.headers().get(axum::http::header::ALLOW) {
        cell.push_str(" allow=");
        cell.push_str(allow.to_str().expect("allow header is ASCII"));
    }
    let mut names: Vec<&str> = response
        .headers()
        .keys()
        .map(|name| name.as_str())
        .filter(|name| !matches!(*name, "allow" | "date"))
        .collect();
    names.sort_unstable();
    if !names.is_empty() {
        cell.push_str(" h=");
        cell.push_str(&names.join(","));
    }
    cell
}

fn candidates() -> Vec<&'static str> {
    INVENTORY
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(|line| line.split(" | ").next().expect("path column"))
        .filter(|path| !path.ends_with(" @ws"))
        .collect()
}

fn probe_paths(candidate: &str) -> Vec<String> {
    let flat = concrete(candidate);
    match flat.strip_prefix("/v1/") {
        Some(rest) => vec![flat.clone(), format!("/v1/workspaces/probe-ws/{rest}")],
        None => vec![flat],
    }
}

#[test]
fn runtime_process_route_inventory_is_pinned() {
    awaken_service_lifecycle::run_composed_async_test(|| async {
        let iam_root = tempfile::tempdir().expect("IAM root");
        let mut listeners: Vec<Router> = Vec::new();
        let mut lifecycles = Vec::new();
        for shape in [Shape::AllInOne, Shape::SecuredAllInOne, Shape::Coordinator] {
            let secured = matches!(shape, Shape::SecuredAllInOne);
            let routers = assemble(shape, iam_root.path()).await;
            listeners.push(routers.public_router.clone());
            if !secured {
                // IAM guards only the public listener; the private one is
                // identical to the unsecured AllInOne column.
                listeners.push(routers.private_router.clone());
            }
            lifecycles.push(routers.service_lifecycle.clone());
        }
        let mut actual = String::from(HEADER);
        for candidate in candidates() {
            for path in probe_paths(candidate) {
                let mut cells = vec![if path == concrete(candidate) {
                    candidate.to_string()
                } else {
                    format!("{candidate} @ws")
                }];
                for listener in &listeners {
                    cells.push(probe(listener, &path).await);
                }
                actual.push_str(&cells.join(" | "));
                actual.push('\n');
            }
        }
        for lifecycle in lifecycles {
            lifecycle
                .shutdown(Duration::from_secs(5))
                .await
                .expect("drain the probed process");
        }
        if std::env::var_os("AWAKEN_WRITE_ROUTE_INVENTORY").is_some() {
            std::fs::write(INVENTORY_PATH, &actual).expect("write route inventory");
            return;
        }
        let expected: String = INVENTORY.lines().map(|line| format!("{line}\n")).collect();
        if expected != actual {
            let diff: Vec<String> = expected
                .lines()
                .zip(actual.lines())
                .filter(|(left, right)| left != right)
                .map(|(left, right)| format!("- {left}\n+ {right}"))
                .collect();
            panic!(
                "route inventory changed ({} differing lines, {} expected vs {} actual lines):\n{}",
                diff.len(),
                expected.lines().count(),
                actual.lines().count(),
                diff.join("\n")
            );
        }
    });
}
