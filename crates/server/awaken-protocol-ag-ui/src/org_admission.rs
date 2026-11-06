//! AG-UI route classification for the access edge's Org admission.
//! Coordinator selects the shared plain-text HTTP encoding; this adapter owns
//! which of its routes consume each bucket, not another copy of the renderer.

use awaken_tenancy::org_admission::OrgOperation;
use axum::http::Method;

/// Starting a Run consumes the Create bucket; reading a Thread consumes the
/// Read bucket; an interrupt passes through unmetered.
#[must_use]
pub fn org_admission_class(method: &Method, path: &str) -> Option<(OrgOperation, &'static str)> {
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    match segments.as_slice() {
        ["v1", "ag-ui"] | ["v1", "ag-ui", "agents", _] if method == Method::POST => {
            Some((OrgOperation::Create, "runs"))
        }
        ["v1", "ag-ui", "threads", _, "messages"]
            if method == Method::GET || method == Method::HEAD =>
        {
            Some((OrgOperation::Read, "threads"))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ag_ui_routes_classify_into_org_buckets() {
        // | Rule | route | effect |
        // | S1 | POST run / Agent-scoped run | Create |
        // | S2 | GET/HEAD Thread messages | Read |
        // | S3 | DELETE Thread runs (interrupt), other paths | unmetered |
        for path in ["/v1/ag-ui", "/v1/ag-ui/agents/a_1"] {
            assert_eq!(
                org_admission_class(&Method::POST, path),
                Some((OrgOperation::Create, "runs")),
                "S1 {path}"
            );
        }
        for method in [Method::GET, Method::HEAD] {
            assert_eq!(
                org_admission_class(&method, "/v1/ag-ui/threads/t_1/messages"),
                Some((OrgOperation::Read, "threads")),
                "S2"
            );
        }
        assert_eq!(
            org_admission_class(&Method::DELETE, "/v1/ag-ui/threads/t_1/runs"),
            None,
            "S3"
        );
        assert_eq!(
            org_admission_class(&Method::GET, "/v1/ai-sdk/x"),
            None,
            "S3"
        );
    }
}
