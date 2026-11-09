// @formal surface requirement=runtime.permission.authority_never_widens
// @formal reason: Session ownership/run admission does not grant definition reads;
// @formal reason: official full DTOs retain their schema behind independent current IAM.
//! Fine definition admission for canonical Managed Session DTOs. The separate
//! private alias projects a manifest in the original Managed serialization owner.
use super::{
    AgentGovernanceConfig,
    authority::{self, Failure},
    routes::RequestContext,
};
use crate::RemoteManagementAuthz;
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{Method, header},
    middleware::Next,
    response::Response,
};
use serde_json::Value;
use std::{collections::BTreeSet, sync::Arc};
use tower::ServiceExt as _;
mod admission;
pub(super) use admission::DefinitionAdmission;
const MAXIMUM_BODY: usize = 2 * 1024 * 1024;
use crate::authz::CurrentManagedDefinitionRequest;
#[derive(Clone)]
struct Ingress {
    iam: Arc<RemoteManagementAuthz>,
    config: Option<AgentGovernanceConfig>,
    original: Router,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Presentation {
    Session,
    Sessions,
    Thread,
    Threads,
}
fn classify(method: &Method, path: &str) -> Option<(Presentation, Option<String>)> {
    let rest = path.strip_prefix("/v1/sessions")?;
    if rest.is_empty() {
        return match *method {
            Method::GET => Some((Presentation::Sessions, None)),
            Method::POST => Some((Presentation::Session, None)),
            _ => None,
        };
    }
    let segments = rest.strip_prefix('/')?.split('/').collect::<Vec<_>>();
    if segments.iter().any(|s| s.is_empty()) {
        return None;
    }
    match (method, segments.as_slice()) {
        (&Method::GET, [_]) => Some((Presentation::Session, None)),
        (&Method::GET, [_, "threads"]) => Some((Presentation::Threads, None)),
        (&Method::GET, [_, "threads", _]) => Some((Presentation::Thread, None)),
        (&Method::POST, [_]) => Some((Presentation::Session, Some(path.into()))),
        (&Method::POST, [_, "archive"]) => Some((
            Presentation::Session,
            Some(path.trim_end_matches("/archive").into()),
        )),
        (&Method::POST, [_, "threads", _, "archive"]) => Some((
            Presentation::Thread,
            Some(path.trim_end_matches("/archive").into()),
        )),
        _ => None,
    }
}
/// Wrap the original Managed graph; it retains independent run/Session
/// authorization. This does not grant auditors any conversation access.
pub(crate) fn protect(
    router: Router,
    iam: Option<Arc<RemoteManagementAuthz>>,
    config: Option<AgentGovernanceConfig>,
) -> Router {
    if config.is_none() {
        return router;
    }
    let Some(iam) = iam else { return router };
    router.clone().layer(axum::middleware::from_fn_with_state(
        Ingress {
            iam,
            config,
            original: router,
        },
        guard,
    ))
}
async fn guard(State(state): State<Ingress>, request: Request, next: Next) -> Response {
    // A WorkQueue-admitted engine request retains the separate native runtime
    // contract. This stamp is never reconstructed from a role or HTTP header.
    if crate::authz::has_work_session_access(&request) {
        return next.run(request).await;
    }
    let Some((presentation, lookup)) = classify(request.method(), request.uri().path()) else {
        return next.run(request).await;
    };
    let mut response = match admitted(state, request, next, presentation, lookup).await {
        Ok(response) => response,
        Err(failure) => super::routes::managed_failure(failure),
    };
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}
async fn check_response(
    admission: &DefinitionAdmission,
    response: Response,
    presentation: Presentation,
    context: &RequestContext,
) -> Result<Response, Failure> {
    if !response.status().is_success() {
        // This observation also covers a rejected/error response after an
        // underlying persistent attempt. The caller maps a failed ending guard
        // to unknown for writes, retaining the native original intent/key.
        admission.end(context).await?;
        return Ok(response);
    }
    let (mut parts, body) = response.into_parts();
    let bytes = to_bytes(body, MAXIMUM_BODY)
        .await
        .map_err(|_| Failure::Unavailable)?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| Failure::Unavailable)?;
    for id in definition_ids(&value, presentation)? {
        if !admission.allows(&id, authority::DEFINITION_READ).await? {
            return Err(Failure::Denied);
        }
    }
    admission.end(context).await?;
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    Ok(Response::from_parts(parts, Body::from(bytes)))
}
async fn admitted(
    state: Ingress,
    mut request: Request,
    next: Next,
    presentation: Presentation,
    lookup: Option<String>,
) -> Result<Response, Failure> {
    let context = RequestContext::from_request(&request)?;
    let admission = DefinitionAdmission::begin(state.iam, state.config.as_ref(), &context).await?;
    let qualifier = CurrentManagedDefinitionRequest::new(
        admission.principal.clone(),
        context.workspace.clone(),
        admission.fence().to_owned(),
    );
    let writing = request.method() != Method::GET;
    if let Some(path) = lookup {
        // Reuse the canonical read and its independent run/owner guard before
        // a mutation that returns a full definition. No duplicate Session store
        // or guessed current Agent is introduced here.
        let mut probe = Request::builder()
            .method(Method::GET)
            .uri(path)
            .body(Body::empty())
            .map_err(|_| Failure::Invalid)?;
        *probe.headers_mut() = request.headers().clone();
        probe.headers_mut().remove(header::CONTENT_LENGTH);
        probe
            .extensions_mut()
            .insert(awaken_authz_enforce::RequestTenancy {
                workspace_id: context.workspace.clone(),
            });
        probe
            .extensions_mut()
            .insert(awaken_runtime_contract::runs::scope::WorkspaceScope(
                context.workspace.clone(),
            ));
        if let Some(purpose) = context.purpose.as_ref() {
            probe.extensions_mut().insert(purpose.clone());
        }
        probe.extensions_mut().insert(qualifier.clone());
        let response = state
            .original
            .oneshot(probe)
            .await
            .unwrap_or_else(|error| match error {});
        if !response.status().is_success() {
            return Err(if response.status().is_server_error() {
                Failure::Unavailable
            } else {
                Failure::Denied
            });
        }
        check_response(&admission, response, presentation, &context).await?;
    } else if writing {
        let (parts, body) = request.into_parts();
        let bytes = to_bytes(body, MAXIMUM_BODY)
            .await
            .map_err(|_| Failure::Invalid)?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| Failure::Invalid)?;
        let id = match value.get("agent") {
            Some(Value::String(id)) => Some(id.as_str()),
            Some(Value::Object(agent)) => agent.get("id").and_then(Value::as_str),
            _ => None,
        }
        .filter(|id| authority::exact(id, 300))
        .ok_or(Failure::Denied)?;
        // The legacy create responds with a full Agent snapshot. Use-only
        // people use the separate private manifest surface.
        if !admission.allows(id, authority::USE).await?
            || !admission.allows(id, authority::DEFINITION_READ).await?
        {
            return Err(Failure::Denied);
        }
        request = Request::from_parts(parts, Body::from(bytes));
    }
    request.extensions_mut().insert(qualifier);
    let response = next.run(request).await;
    check_response(&admission, response, presentation, &context)
        .await
        .map_err(|failure| {
            if writing {
                Failure::Unavailable
            } else {
                failure
            }
        })
}
fn definition_ids(value: &Value, presentation: Presentation) -> Result<BTreeSet<String>, Failure> {
    let mut ids = BTreeSet::new();
    match presentation {
        Presentation::Session | Presentation::Thread => {
            add_agent(value.get("agent").ok_or(Failure::Unavailable)?, &mut ids)?
        }
        Presentation::Sessions | Presentation::Threads => {
            for row in value
                .get("data")
                .and_then(Value::as_array)
                .ok_or(Failure::Unavailable)?
            {
                add_agent(row.get("agent").ok_or(Failure::Unavailable)?, &mut ids)?;
            }
        }
    }
    Ok(ids)
}
fn add_agent(value: &Value, ids: &mut BTreeSet<String>) -> Result<(), Failure> {
    if value.get("type").and_then(Value::as_str) == Some("advisor") {
        return Ok(());
    }
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| authority::exact(id, 300))
        .ok_or(Failure::Unavailable)?;
    ids.insert(id.into());
    if ids.len() > 4096 {
        return Err(Failure::Unavailable);
    }
    if let Some(roster) = value.get("multiagent").filter(|v| !v.is_null()) {
        for child in roster
            .get("agents")
            .and_then(Value::as_array)
            .ok_or(Failure::Unavailable)?
        {
            add_agent(child, ids)?;
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn full_session_collects_frozen_child_definitions_but_never_parses_message_or_metadata_agent_fields()
     {
        let body = json!({"agent":{"id":"root","multiagent":{"agents":[{"id":"child"},{"name":"review","type":"advisor"}]}},"metadata":{"agent":{"id":"user-owned"}},"content":{"agent":{"id":"tool-result"}}});
        assert_eq!(
            definition_ids(&body, Presentation::Session).unwrap(),
            BTreeSet::from(["root".into(), "child".into()])
        );
        assert!(
            definition_ids(
                &json!({"agent":{"multiagent":{"agents":[]}}}),
                Presentation::Session
            )
            .is_err()
        );
        assert!(definition_ids(&json!({"data":{}}), Presentation::Sessions).is_err());
    }
    #[test]
    fn every_legacy_full_definition_response_is_classified_including_mutation_receipts() {
        for path in [
            "/v1/sessions/s",
            "/v1/sessions/s/archive",
            "/v1/sessions/s/threads/t/archive",
        ] {
            assert!(classify(&Method::POST, path).unwrap().1.is_some(), "{path}");
        }
        for path in [
            "/v1/sessions/s/events",
            "/v1/sessions/s/events/stream",
            "/v1/sessions/s/threads/t/events",
            "/v1/sessions/s/threads/t/stream",
        ] {
            assert_eq!(classify(&Method::GET, path), None);
        }
    }
}
