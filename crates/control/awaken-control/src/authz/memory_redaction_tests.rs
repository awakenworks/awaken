//! T25-C01 (归属与权限), permission row: redacting an immutable Memory
//! version is a distinct authority from writing Memory content.
//!
//! | Row | Route | Expected action |
//! |---|---|---|
//! | R1 | GET a version / the version log | the read action of the Memory family |
//! | R2 | POST a Memory (content write) | the content-write action |
//! | R3 | POST `.../memory_versions/{vid}/redact` | an action a content-only writer does NOT hold |
//!
//! R3 was a product gap (redaction mapped to the same `workspace.write` a
//! content writer holds); ADR-0103 gives it its own `workspace.memory_redact`
//! action, which `workspace.write` alone does not grant.
use super::{Method, RouteAuthz, WORKSPACE_READ, WORKSPACE_WRITE, action_for};

const VERSION: &str = "/v1/memory_stores/memstore_t25/memory_versions/memver_t25";
const CONTENT: &str = "/v1/memory_stores/memstore_t25/memories";

fn resource_action(method: &Method, path: &str) -> Option<&'static str> {
    match action_for(method, path) {
        Some(RouteAuthz::Resource { action, .. }) => Some(action),
        _ => None,
    }
}

#[test]
fn memory_version_reads_and_content_writes_use_the_memory_family_actions() {
    // R1
    for path in [
        VERSION.to_string(),
        "/v1/memory_stores/memstore_t25/memory_versions".to_string(),
    ] {
        assert_eq!(
            resource_action(&Method::GET, &path),
            Some(WORKSPACE_READ),
            "R1 {path}"
        );
    }
    // R2
    assert_eq!(
        resource_action(&Method::POST, CONTENT),
        Some(WORKSPACE_WRITE),
        "R2 content write"
    );
}

#[test]
fn redacting_a_version_requires_more_than_content_write() {
    let redact = resource_action(&Method::POST, &format!("{VERSION}/redact"));
    let content_write = resource_action(&Method::POST, CONTENT);
    assert!(
        redact.is_some(),
        "R3 redaction is an authorized resource route"
    );
    assert_ne!(
        redact, content_write,
        "R3 a principal granted only content write must not be able to redact history"
    );
}
